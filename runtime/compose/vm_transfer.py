"""Private worker-side cold-transfer storage; never exposed to tenant RPCs.

Only controller-created migration IDs select paths. The authority owns node
bindings and progression. Incoming disks remain fenced and unmetered until a
separate, fresh admission step completes after authority handoff.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import threading

from microvm import VMError, atomic_json

CHUNK = 512 * 1024
COMMON_FILES = ('disk.qcow2', 'client_key', 'client_key.pub', 'known_hosts',
                'seed/ssh_host_ed25519_key.pub', 'seed/authorized_keys',
                'seed/runtime.json', 'meta.json')
FILES = COMMON_FILES + ('seed.ext4', 'seed/ssh_host_ed25519_key')
ENCRYPTED_FILES = COMMON_FILES + ('seed.ext4.enc', 'seed/ssh_host_ed25519_key.enc')
ALLOWED_FILES = set(FILES) | set(ENCRYPTED_FILES)


def transfer_files(meta):
    return ENCRYPTED_FILES if meta.get('disk_encryption') else FILES


def sha(path):
    h=hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda:f.read(1024*1024),b''):h.update(chunk)
    return h.hexdigest()


def unpack(archive, folder, expected=FILES):
    """No extractall: reject links, duplicate members and unbounded metadata."""
    seen=set()
    with tarfile.open(archive,'r:') as stream:
        for member in stream:
            if member.name not in ALLOWED_FILES or member.name in seen or not member.isfile():
                raise VMError('invalid_migration_archive')
            maximum=101*1024**3 if member.name=='disk.qcow2' else 8*1024**2
            if not 0 <= member.size <= maximum:raise VMError('migration_file_too_large')
            seen.add(member.name)
            target=folder/member.name
            target.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
            with stream.extractfile(member) as src, target.open('xb') as dst:
                shutil.copyfileobj(src,dst,1024*1024)
                dst.flush();os.fsync(dst.fileno())
            target.chmod(0o600)
    if expected is not None and seen!=set(expected):raise VMError('migration_archive_incomplete')
    return seen


class Transfers:
    def __init__(self,runner):
        self.runner=runner;self.manager=runner.hypervisor
        self.root=self.manager.root/'transfers'
        self.root.mkdir(mode=0o700,exist_ok=True)
        self.guard=threading.RLock();self.tasks={}

    def folder(self,identity):
        if not isinstance(identity,str) or not re.fullmatch('move_[0-9a-f]{32}',identity):
            raise VMError('invalid_transfer_id')
        folder=self.root/identity;folder.mkdir(mode=0o700,exist_ok=True)
        return folder

    def authority(self,identity,side=None):
        ledger=self.runner.runtime.ledger
        if not hasattr(ledger,'transport'):raise VMError('migration_requires_fleet_accounting')
        row=ledger.transport(dict(action='migration-status',migration_id=identity))
        if row.get('migration_id')!=identity:raise VMError('migration_binding_mismatch')
        if side and row.get(side+'_node')!=ledger.config['node_id']:
            raise VMError('migration_node_mismatch')
        return row

    def operation(self,body):
        identity=body['migration_id'];folder=self.folder(identity);action=body['operation']
        with self.guard:
            row=self.authority(identity)
            if action=='binding':
                meta=self.manager.read(row['project_id'],row['owner_did'],row['vm_id'])
                target=folder/'target.json'
                if not meta and target.exists():meta=json.loads(target.read_text())['meta']
                return dict(row,vm=self.manager.public(meta) if meta else None,
                    route_key=(meta or {}).get('catalog_key',row['project_id']),
                    ingress=self.runner.ingress.public(meta) if self.runner.ingress and meta else {},
                    kernel_sha256=sha(self.manager.images/'vmlinuz'),
                    free_bytes=shutil.disk_usage(folder).free,
                    memory_available_mib=next(int(line.split()[1]) for line in Path('/proc/meminfo').read_text().splitlines() if line.startswith('MemAvailable:'))//1024,
                    cpu_flags=next((line.split(':',1)[1].split() for line in Path('/proc/cpuinfo').read_text().splitlines() if line.startswith('flags')),[]))
            if action=='route':return self.route(identity,body)
            if action=='prune':
                self.authority(identity,'target')
                if row['phase']!='committed' or not (folder/'activated.json').exists():raise VMError('migration_target_not_activated')
                for name in ('import.tar','upload.json'):
                    path=folder/name
                    if path.exists():path.unlink()
                return dict(phase='pruned')
            if action=='policy':
                if row.get('home_node')!=self.runner.runtime.ledger.config['node_id']:
                    raise VMError('migration_origin_node_required')
                return dict(approval=self.runner.authorize_local(row['project_id'],row['owner_did']),
                    policy=self.runner.workload_policies_local([row['project_id']]).get(row['project_id'],{}))
            if action=='status':
                state=folder/'state.json'
                return json.loads(state.read_text()) if state.exists() else {'phase':'absent'}
            if action=='upload-status':
                self.authority(identity,'target')
                if row['phase']!='source_fenced':raise VMError('migration_phase_conflict')
                manifest=folder/'upload.json';archive=folder/'import.tar'
                if not manifest.exists():return dict(received=0)
                return dict(json.loads(manifest.read_text()),received=archive.stat().st_size if archive.exists() else 0)
            if action in ('settle','activate','restore','discard','cleanup'):
                side='target' if action in ('activate','discard') else 'source'
                row=self.authority(identity,side)
                allowed={'settle':('target_staged','source_settled','routing_ready','committed'),'activate':('committed',),'restore':('target_discarded','cancelled'),'discard':('cancelling','target_discarded','cancelled'),'cleanup':('committed',)}[action]
                if row['phase'] not in allowed:raise VMError('migration_phase_conflict')
                if identity in self.tasks:return {'phase':'working','operation':action}
                # Activation must recheck current capacity even after an earlier
                # success: this VM might since have moved again.
                atomic_json(folder/'state.json',dict(phase='working',operation=action))
                thread=threading.Thread(target=self.run,args=(identity,action),daemon=True)
                self.tasks[identity]=thread;thread.start()
                return {'phase':'working','operation':action}
            if action in ('export','import'):
                state=folder/'state.json'
                old=json.loads(state.read_text()) if state.exists() else {}
                self.authority(identity,'source' if action=='export' else 'target')
                allowed=('prepared','source_fenced','target_staged') if action=='export' else ('source_fenced','target_staged')
                if row['phase'] not in allowed:raise VMError('migration_phase_conflict')
                if old.get('phase')==action+'ed':return old
                # Recover an acknowledgement lost after the authority committed it.
                if action=='export' and row['phase'] in ('source_fenced','target_staged') and (folder/'receipt.json').exists():
                    receipt=json.loads((folder/'receipt.json').read_text())
                    if receipt['disk_sha256']!=row['disk_sha256']:raise VMError('migration_disk_digest_mismatch')
                    result=dict(phase='exported',**receipt);atomic_json(state,result);return result
                if action=='import' and row['phase']=='target_staged' and (folder/'incoming/.migration-fence').exists():
                    result=dict(phase='imported',vm_id=row['vm_id'],disk_sha256=row['disk_sha256'],execution_authorized=False)
                    atomic_json(state,result);return result
                if identity in self.tasks:return {'phase':action+'ing'}
                self.authority(identity,'source' if action=='export' else 'target')
                if row['phase']!=('prepared' if action=='export' else 'source_fenced'):
                    raise VMError('migration_phase_conflict')
                atomic_json(state,dict(phase=action+'ing'))
                thread=threading.Thread(target=self.run,args=(identity,action),daemon=True)
                self.tasks[identity]=thread;thread.start()
                return {'phase':action+'ing'}
            if action=='read':
                if row['source_node']!=self.runner.runtime.ledger.config['node_id']:raise VMError('migration_node_mismatch')
                if row['phase'] not in ('source_fenced','target_staged'):raise VMError('migration_phase_conflict')
                offset=body['offset']
                if type(offset) is not int or offset<0:raise VMError('invalid_transfer_offset')
                path=folder/'export.tar'
                if offset>path.stat().st_size:raise VMError('invalid_transfer_offset')
                with path.open('rb') as f:f.seek(offset);data=f.read(CHUNK)
                return dict(offset=offset,data_base64=base64.b64encode(data).decode(),size=path.stat().st_size)
            if action=='write':
                if row['target_node']!=self.runner.runtime.ledger.config['node_id']:raise VMError('migration_node_mismatch')
                if row['phase']!='source_fenced':raise VMError('migration_phase_conflict')
                offset=body['offset'];size=body['size'];digest=body['archive_sha256']
                if type(offset) is not int or offset<0 or type(size) is not int or not 0<size<=102*1024**3:
                    raise VMError('invalid_transfer_offset')
                if not isinstance(digest,str) or not re.fullmatch('[0-9a-f]{64}',digest):raise VMError('invalid_transfer_digest')
                data=base64.b64decode(body['data_base64'],validate=True)
                if not 0<len(data)<=CHUNK or offset+len(data)>size:raise VMError('invalid_transfer_chunk')
                manifest=folder/'upload.json';expected=dict(size=size,archive_sha256=digest)
                if manifest.exists() and json.loads(manifest.read_text())!=expected:raise VMError('migration_upload_conflict')
                if not manifest.exists():
                    # Archive plus unpacked disk coexist until validation. Leave
                    # room for metadata; installation later renames, not copies.
                    if shutil.disk_usage(folder).free < 2*size+64*1024**2:
                        raise VMError('migration_target_disk_space_insufficient')
                    atomic_json(manifest,expected)
                path=folder/'import.tar';current=path.stat().st_size if path.exists() else 0
                if offset<current:
                    with path.open('rb') as f:f.seek(offset);old=f.read(len(data))
                    if old!=data:raise VMError('migration_chunk_conflict')
                else:
                    if offset!=current:raise VMError('migration_chunk_out_of_order')
                    with path.open('ab') as f:f.write(data);f.flush();os.fsync(f.fileno())
                return dict(received=path.stat().st_size,size=size)
            raise VMError('invalid_transfer_operation')

    def run(self,identity,action):
        folder=self.folder(identity)
        try:
            result={'export':self.export,'import':self.receive,'settle':self.settle,'activate':self.activate,'restore':self.restore,'discard':self.discard,'cleanup':self.cleanup}[action](identity)
            result=dict(result)
            result.setdefault('phase',action+'ed')
            result['operation']=action
            atomic_json(folder/'state.json',result)
        except Exception as error:
            # Do not echo subprocess output, archive names or credentials.
            code=str(error) if isinstance(error,VMError) else 'migration_worker_failed'
            atomic_json(folder/'state.json',dict(phase='failed',error=code))
        finally:
            with self.guard:self.tasks.pop(identity,None)

    def export(self,identity):
        m=self.manager;row=self.authority(identity,'source');folder=self.folder(identity)
        if row['phase']!='prepared':raise VMError('migration_phase_conflict')
        project=row['project_id'];owner=row['owner_did'];vm=row['vm_id']
        with self.runner.runtime.lock(project):
            meta=m.read(project,owner,vm)
            if not meta or meta['state']=='destroyed':raise VMError('migration_vm_missing')
            self.runner.authorize(project,owner)
            if self.runner.runtime.ledger.view(project,owner,include_entries=False)['budget_microcredits'] is not None:
                raise VMError('migration_project_budget_requires_central_support')
            restore_running=meta['state'] in ('running','hibernated')
            if not (folder/'intent.json').exists():atomic_json(folder/'intent.json',dict(restore_running=restore_running))
            if meta['state']=='hibernated':
                self.runner.runtime.check_credit(meta)
                with self.runner.runtime.admission(project,meta['vcpus'],meta['memory_mib'],vm):
                    m.perform(project,owner,'vm/resume',dict(vm_id=vm,request_id=identity+':resume'))
            m.fence_for_migration(project,owner,vm,identity)
            meta=m.read(project,owner,vm)
            self.runner.runtime.sample(meta)
            if self.runner.ingress:self.runner.ingress.sync()
            bundle=folder/'export'
            if bundle.exists():shutil.rmtree(bundle)
            bundle.mkdir(mode=0o700)
            m.disk_crypto.execute(meta,'convert',m.folder(meta)/'disk.qcow2',output=bundle/'disk.qcow2')
            m.seed_crypto.protect(meta,m.folder(meta))
            files=transfer_files(meta)
            info=json.loads(subprocess.check_output(['qemu-img','info','--output=json',str(bundle/'disk.qcow2')],text=True))
            if 'backing-filename' in info:raise VMError('migration_disk_not_standalone')
            if shutil.disk_usage(folder).free < (bundle/'disk.qcow2').stat().st_size+64*1024**2:
                raise VMError('migration_source_disk_space_insufficient')
            for name in files:
                if name in ('disk.qcow2','meta.json'):continue
                dest=bundle/name;dest.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
                shutil.copy2(m.folder(meta)/name,dest)
            atomic_json(bundle/'meta.json',dict(meta=meta,kernel_sha256=sha(m.images/'vmlinuz'),origin_url=meta.get('ingress_origin',m.ingress_origin),http_origin_node=meta.get('http_origin_node',self.runner.runtime.ledger.config['node_id']),restore_running=json.loads((folder/'intent.json').read_text())['restore_running']))
            disk_digest=sha(bundle/'disk.qcow2')
            temporary=folder/'export.next'
            with tarfile.open(temporary,'w') as tar:
                for name in files:tar.add(bundle/name,arcname=name,recursive=False)
            with temporary.open('rb') as f:os.fsync(f.fileno())
            temporary.replace(folder/'export.tar')
            result=dict(size=(folder/'export.tar').stat().st_size,archive_sha256=sha(folder/'export.tar'),disk_sha256=disk_digest)
            atomic_json(folder/'receipt.json',result)
            self.runner.runtime.ledger.transport(dict(action='migration-attest',request_id=identity+':source',migration_id=identity,revision=row['revision'],stage='source-fenced',evidence_id=identity+':export',disk_sha256=disk_digest))
            return result

    def receive(self,identity):
        m=self.manager;row=self.authority(identity,'target');folder=self.folder(identity)
        if row['phase']!='source_fenced':raise VMError('migration_phase_conflict')
        upload=json.loads((folder/'upload.json').read_text());archive=folder/'import.tar'
        if archive.stat().st_size!=upload['size'] or sha(archive)!=upload['archive_sha256']:
            raise VMError('migration_archive_digest_mismatch')
        staging=folder/'incoming'
        if staging.exists():shutil.rmtree(staging)
        staging.mkdir(mode=0o700);seen=unpack(archive,staging,expected=None)
        receipt=json.loads((staging/'meta.json').read_text());meta=receipt['meta']
        if seen!=set(transfer_files(meta)):raise VMError('migration_archive_incomplete')
        if (meta['vm_id'],meta['project_id'],meta['owner_did'])!=(row['vm_id'],row['project_id'],row['owner_did']):
            raise VMError('migration_binding_mismatch')
        if receipt['kernel_sha256']!=sha(m.images/'vmlinuz'):raise VMError('migration_kernel_mismatch')
        if sha(staging/'disk.qcow2')!=row['disk_sha256']:raise VMError('migration_disk_digest_mismatch')
        info=json.loads(subprocess.check_output(['qemu-img','info','--output=json',str(staging/'disk.qcow2')],text=True))
        if info.get('format')!='qcow2' or 'backing-filename' in info or info.get('virtual-size')!=meta['disk_gib']*1024**3:
            raise VMError('migration_disk_format_mismatch')
        m.disk_crypto.execute(meta,'check',staging/'disk.qcow2')
        # Keep validated data outside the live catalog. Activation will install
        # fresh host ports, capacity intents and billing bindings after handoff.
        atomic_json(staging/'.migration-fence',dict(transfer_id=identity,vm_id=meta['vm_id']))
        atomic_json(folder/'target.json',receipt)
        self.runner.runtime.ledger.transport(dict(action='migration-attest',request_id=identity+':target',migration_id=identity,revision=row['revision'],stage='target-staged',evidence_id=identity+':import',disk_sha256=row['disk_sha256']))
        return dict(vm_id=meta['vm_id'],disk_sha256=row['disk_sha256'],execution_authorized=False)

    def route(self,identity,body):
        row=self.authority(identity,'source');m=self.manager
        if row['phase'] not in ('source_settled','routing_ready'):raise VMError('migration_phase_conflict')
        config=json.loads(Path(self.runner.path).read_text())
        peer=config.get('migration_peers',{}).get(row['target_node'])
        if not peer:raise VMError('migration_peer_not_configured')
        username=body.get('username');password=body.get('password')
        if not isinstance(username,str) or not re.fullmatch('[A-Za-z0-9_-]{1,64}',username):raise VMError('invalid_migration_route_credentials')
        if not isinstance(password,str) or not 12<=len(password.encode())<=128 or any(ord(c)<32 for c in password):raise VMError('invalid_migration_route_credentials')
        meta=m.read(row['project_id'],row['owner_did'],row['vm_id'])
        fence=m.folder(meta)/'.migration-fence'
        if not fence.exists() or json.loads(fence.read_text()).get('transfer_id')!=identity or m.alive(meta):raise VMError('migration_source_not_fenced')
        path=m.root/'migration-routes.json';routes=json.loads(path.read_text()) if path.exists() else {}
        entry=dict(migration_id=identity,project_id=row['project_id'],owner_did=row['owner_did'],
            route_key=meta.get('catalog_key',row['project_id']),origin=peer['origin'],username=username,password=password,forwarded=meta.get('migration_http_hop',False))
        routes[row['vm_id']]=entry;atomic_json(path,routes)
        if not self.runner.ingress:raise VMError('migration_ingress_required')
        self.runner.ingress.sync()
        return dict(phase='routes_ready',evidence_id=identity+':proxy')

    def remove_route(self,identity):
        path=self.manager.root/'migration-routes.json'
        with self.guard:
            values=json.loads(path.read_text()) if path.exists() else {}
            values={vm:entry for vm,entry in values.items() if entry['migration_id']!=identity}
            if path.exists():atomic_json(path,values)
        if self.runner.ingress:self.runner.ingress.sync()

    def cleanup(self,identity):
        row=self.authority(identity,'source');m=self.manager;folder=self.folder(identity)
        if row['phase']!='committed':raise VMError('migration_handoff_required')
        # Coordinator requests cleanup only after target HTTP and runtime checks;
        # independently require target's persisted activation receipt as well.
        from migration_peer import request
        reply=request(json.loads(Path(self.runner.path).read_text()),row['target_node'],
            dict(operation='status',migration_id=identity))
        if reply.get('phase')!='activated':raise VMError('migration_target_not_activated')
        with self.runner.runtime.lock(row['project_id']):
            meta=m.read(row['project_id'],row['owner_did'],row['vm_id'])
            fence=m.folder(meta)/'.migration-fence'
            if not fence.exists() or json.loads(fence.read_text()).get('transfer_id')!=identity or m.alive(meta):raise VMError('migration_source_not_fenced')
            meta.update(state='migrated',outgoing_migration=identity,migrated_to=row['target_node'],ports=[],public_ports=[],public_mappings=[],public_targets={})
            m.save(meta)
            for path in m.folder(meta).iterdir():
                if path.name in ('.migration-fence','.migration-meter-off'):continue
                if path.is_dir():shutil.rmtree(path)
                else:path.unlink()
            for name in ('export','export.tar','export.next'):
                path=folder/name
                if path.is_dir():shutil.rmtree(path)
                elif path.exists():path.unlink()
            if self.runner.ingress:self.runner.ingress.sync()
            return dict(phase='cleaned')

    def discard(self,identity):
        """Acknowledge cancellation only after every staged target copy is gone."""
        row=self.authority(identity,'target');folder=self.folder(identity)
        if row['phase'] in ('target_discarded','cancelled'):
            return dict(phase='discarded')
        if row['phase']!='cancelling':raise VMError('migration_phase_conflict')
        with self.runner.runtime.lock(row['project_id']):
            if any(m['vm_id']==row['vm_id'] and m['state']!='destroyed'
                   for m in self.manager.list(row['project_id'],row['owner_did'])):
                raise VMError('migration_target_collision')
            for name in ('incoming','install','import.tar','upload.json','target.json'):
                path=folder/name
                if path.is_dir():shutil.rmtree(path)
                elif path.exists():path.unlink()
            fd=os.open(folder,os.O_RDONLY|os.O_DIRECTORY)
            try:os.fsync(fd)
            finally:os.close(fd)
            self.runner.runtime.ledger.transport(dict(action='migration-cancel-attest',
                request_id=identity+':discard',migration_id=identity,revision=row['revision'],
                stage='target-discarded',evidence_id=identity+':discarded'))
            atomic_json(folder/'state.json',dict(phase='discarded'))
            return dict(phase='discarded')

    def restore(self,identity):
        row=self.authority(identity,'source');folder=self.folder(identity)
        if row['phase']=='cancelled':return dict(phase='restored')
        if row['phase']!='target_discarded':raise VMError('migration_target_discard_required')
        m=self.manager;runtime=self.runner.runtime;project=row['project_id'];owner=row['owner_did']
        with runtime.lock(project):
            meta=m.read(project,owner,row['vm_id'])
            if not meta or meta['state']=='destroyed':raise VMError('migration_vm_missing')
            self.runner.authorize(project,owner)
            self.remove_route(identity)
            with m.owner_lock(owner):
                fence=m.folder(meta)/'.migration-fence'
                marker=m.folder(meta)/'.migration-meter-off'
                for path in (fence,marker):
                    if path.exists() and json.loads(path.read_text()).get('transfer_id')!=identity:
                        raise VMError('migration_source_collision')
                # The target discard is durable before metering or execution
                # resumes. Rebase samples so the paused interval is not charged.
                if marker.exists():
                    with runtime.ledger.db() as db:db.execute('DELETE FROM samples WHERE vm=?',(row['vm_id'],))
                    marker.unlink()
                if fence.exists():fence.unlink()
                fd=os.open(m.folder(meta),os.O_RDONLY|os.O_DIRECTORY)
                try:os.fsync(fd)
                finally:os.close(fd)
            intent=folder/'intent.json'
            runtime.sample(meta)
            if intent.exists() and json.loads(intent.read_text()).get('restore_running'):
                runtime.check_credit(meta)
                with runtime.admission(project,meta['vcpus'],meta['memory_mib'],row['vm_id']):
                    m.perform(project,owner,'vm/start',dict(vm_id=row['vm_id'],request_id=identity+':restore'))
            meta=m.read(project,owner,row['vm_id']);runtime.sample(meta)
            if runtime.gateway:runtime.gateway.reconcile()
            if self.runner.ingress:self.runner.ingress.sync()
            runtime.ledger.transport(dict(action='migration-cancel-attest',request_id=identity+':restore',
                migration_id=identity,revision=row['revision'],stage='source-restored',evidence_id=identity+':restored'))
            return dict(phase='restored')

    def settle(self,identity):
        row=self.authority(identity,'source');folder=self.folder(identity)
        if row['phase'] in ('source_settled','routing_ready','committed'):
            return dict(phase=row['phase'])
        if row['phase']!='target_staged':raise VMError('migration_phase_conflict')
        m=self.manager;project=row['project_id'];owner=row['owner_did'];ledger=self.runner.runtime.ledger
        with self.runner.runtime.lock(project):
            meta=m.read(project,owner,row['vm_id'])
            fence=m.folder(meta)/'.migration-fence'
            if not fence.exists() or json.loads(fence.read_text()).get('transfer_id')!=identity or m.alive(meta):
                raise VMError('migration_source_not_fenced')
            marker=m.folder(meta)/'.migration-meter-off'
            if not marker.exists():
                self.runner.runtime.sample(meta)
                atomic_json(marker,dict(transfer_id=identity))
            receipt=folder/'settlement.json'
            if not receipt.exists():
                previous=ledger.last_checkpoints.get(project)
                ledger.sync(project,owner,force=True)
                checkpoint=ledger.last_checkpoints.get(project)
                if not checkpoint or checkpoint==previous:raise VMError('migration_settlement_unavailable')
                atomic_json(receipt,dict(checkpoint_request_id=checkpoint))
            checkpoint=json.loads(receipt.read_text())['checkpoint_request_id']
            result=ledger.transport(dict(action='migration-settle',request_id=identity+':settle',migration_id=identity,
                revision=row['revision'],checkpoint_request_id=checkpoint))
            return dict(phase=result['phase'])

    def activate(self,identity):
        row=self.authority(identity,'target');folder=self.folder(identity)
        if row['phase']!='committed':raise VMError('migration_handoff_required')
        m=self.manager;runtime=self.runner.runtime;project=row['project_id'];owner=row['owner_did'];vm=row['vm_id']
        incoming=folder/'incoming'
        receipt=json.loads((folder/'target.json').read_text())
        if receipt['kernel_sha256']!=sha(m.images/'vmlinuz'):raise VMError('migration_kernel_mismatch')
        with runtime.lock(project):
            home=row.get('home_node')
            if not home:raise VMError('migration_origin_node_required')
            if home!=runtime.ledger.config['node_id']:
                origins=m.root/'migration-origins.json'
                with self.guard:
                    values=json.loads(origins.read_text()) if origins.exists() else {}
                    old=values.get(project)
                    if old and (old['node_id'],old['owner_did'])!=(home,owner):
                        raise VMError('migration_origin_binding_mismatch')
                    values[project]=dict(node_id=home,owner_did=owner,migration_id=identity)
                    atomic_json(origins,values)
            self.runner.authorize(project,owner)
            runtime.ledger.adopt_migrated_project(project,owner)
            with m.owner_lock(owner):
                remote=m.capacity.adopt_migrated_vm(project,owner,vm)
                meta=next((r for r in m.list(project,owner) if r['vm_id']==vm),None)
                if meta and meta['state']=='migrated':
                    marker=m.folder(meta)/'.migration-fence'
                    previous=meta.get('outgoing_migration')
                    if not previous or not marker.exists() or json.loads(marker.read_text()).get('transfer_id')!=previous or m.alive(meta):
                        raise VMError('migration_target_collision')
                    self.remove_route(previous)
                    shutil.rmtree(m.folder(meta))
                    m.catalog(meta.get('catalog_key',project)).unlink()
                    meta=None
                if meta is None:
                    meta=dict(receipt['meta'])
                    if m.capacity.resources(meta)!=remote['committed']:raise VMError('migration_resource_mismatch')
                    meta.update(state='stopped',ports=[],image_version=m.image_version(),retained=False,
                                ingress_origin=receipt['origin_url'],http_origin_node=receipt.get('http_origin_node',row.get('home_node')),migration_id=identity,migration_http_hop=receipt['origin_url'].rstrip('/')!=getattr(m,'ingress_origin','').rstrip('/'))
                    for key in ('pid','snapshot_tag','snapshot_qemu_version','public_ports','public_targets','public_mappings'):
                        meta.pop(key,None)
                    with m.allocation_lock():
                        existing=m.catalog(meta.get('catalog_key',project))
                        if existing.exists():raise VMError('migration_catalog_collision')
                        meta['ssh_port']=m.reserved_port();used={meta['ssh_port']}
                        for port in receipt['meta'].get('ports',[]):
                            worker=m.reserved_port(used);used.add(worker)
                            meta['ports'].append(dict(guest_port=port['guest_port'],worker_port=worker))
                        if m.network:
                            m.network.allocate(meta)
                            meta['public_mappings']=receipt['meta'].get('public_mappings',[])
                        target=m.folder(meta)
                        if not target.exists():
                            atomic_json(incoming/'.migration-meter-off',dict(transfer_id=identity))
                            incoming.rename(target)
                            parent_fd=os.open(target.parent,os.O_RDONLY|os.O_DIRECTORY)
                            try:os.fsync(parent_fd)
                            finally:os.close(parent_fd)
                        else:
                            marker=target/'.migration-fence'
                            if not marker.exists() or json.loads(marker.read_text()).get('transfer_id')!=identity:
                                raise VMError('migration_target_collision')
                        m.save(meta)
                elif meta.get('migration_id')!=identity:
                    raise VMError('migration_target_collision')
                # Check the CURRENT authority allocation, never only a cached commit.
                if m.capacity.resources(meta)!=remote['committed']:raise VMError('migration_resource_mismatch')
                m.capacity.ready.add(vm)
                fence=m.folder(meta)/'.migration-fence'
                if fence.exists() and json.loads(fence.read_text()).get('transfer_id')!=identity:
                    raise VMError('migration_target_collision')
                marker=m.folder(meta)/'.migration-meter-off'
                if marker.exists():
                    with runtime.ledger.db() as db:db.execute('DELETE FROM samples WHERE vm=?',(vm,))
                    marker.unlink()
                fence=m.folder(meta)/'.migration-fence'
                if fence.exists():
                    if json.loads(fence.read_text()).get('transfer_id')!=identity:raise VMError('migration_target_collision')
                    fence.unlink()
                # Persist marker removal before any guest execution.
                fd=os.open(m.folder(meta),os.O_RDONLY|os.O_DIRECTORY)
                try:os.fsync(fd)
                finally:os.close(fd)
            runtime.sample(meta)
            if receipt.get('restore_running',True):
                runtime.touch(meta)
                runtime.check_credit(meta)
                with runtime.admission(project,meta['vcpus'],meta['memory_mib'],vm):
                    m.perform(project,owner,'vm/start',dict(vm_id=vm,request_id=identity+':activate'))
            meta=m.read(project,owner,vm)
            runtime.sample(meta)
            if runtime.gateway:runtime.gateway.reconcile()
            if self.runner.ingress:self.runner.ingress.sync()
            atomic_json(folder/'activated.json',dict(vm_id=vm))
            return dict(phase='activated',vm=m.public(meta))
