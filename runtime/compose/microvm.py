"""Managed QEMU/KVM microVMs. Host paths and QMP commands are controller-owned.

No agent-supplied command, host mount, disk path, kernel or SSH target is used.
The runner requires KVM access but not host root, Docker or network-admin rights.
"""
from contextlib import contextmanager
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import threading
import time
import uuid


class VMError(Exception):
    pass


def atomic_json(path, value):
    temporary = path.with_suffix('.tmp')
    with temporary.open('w') as out:
        json.dump(value, out)
        out.flush()
        os.fsync(out.fileno())
    temporary.replace(path)
    directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def run(command):
    # Never execute a shell, inherit secrets or return host command output.
    result = subprocess.run(command, env={'PATH': '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin'},
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                            stderr=subprocess.PIPE, timeout=120)
    if result.returncode:
        error = VMError('hypervisor_command_failed:' + Path(command[0]).name)
        error.diagnostic = result.stderr[-4096:].decode(errors='replace')
        raise error


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def validate(action, body):
    fields = {
        'vm/create': {'request_id', 'vcpus', 'memory_mib', 'disk_gib', 'ports', 'start', 'ssh_keys', 'new_vm', 'execution_mode', 'placement_id'},
        'vm/update': {'request_id', 'vm_id', 'vcpus', 'memory_mib', 'disk_gib', 'ports'},
        'vm/start': {'request_id', 'vm_id'},
        'vm/hibernate': {'request_id', 'vm_id'},
        'vm/resume': {'request_id', 'vm_id'},
        'vm/stop': {'request_id', 'vm_id', 'force'},
        'vm/destroy': {'request_id', 'vm_id', 'delete_data', 'confirm_data_loss'},
    }
    if action not in fields or set(body) - fields[action]:
        raise VMError('invalid_vm_operation_fields')
    if action != 'vm/create' and not re.fullmatch(r'vm_[0-9a-f]{32}', str(body.get('vm_id', ''))):
        raise VMError('vm_id_required')
    if 'placement_id' in body and not re.fullmatch(r'plc_[0-9a-f]{32}', str(body['placement_id'])):
        raise VMError('invalid_placement_id')
    if 'vcpus' in body:
        from cpu_quota import quarters
        try: quarters(body['vcpus'])
        except ValueError: raise VMError('invalid_vcpus')
    for key in ('memory_mib', 'disk_gib'):
        if key in body and (type(body[key]) is not int or not 0 < body[key] < 2**31):
            raise VMError('invalid_' + key)
    if 'memory_mib' in body and (body['memory_mib']<256 or body['memory_mib']%256):
        raise VMError('memory_must_be_multiple_of_256_mib')
    for key,maximum in (('vcpus',4),('memory_mib',8192),('disk_gib',100)):
        if key in body and body[key]>maximum:raise VMError(key+'_exceeds_vm_limit')
    for key in ('start', 'force', 'delete_data', 'confirm_data_loss', 'new_vm'):
        if key in body and type(body[key]) is not bool:
            raise VMError('invalid_' + key)
    if 'ssh_keys' in body:
        from network import keys
        keys(body['ssh_keys'])
    if 'execution_mode' in body and body['execution_mode'] not in ('serverless','always_on'):
        raise VMError('invalid_execution_mode')
    ports = body.get('ports', [])
    if (not isinstance(ports, list) or any(type(p) is not int or not 1 <= p <= 65535 or p == 22 for p in ports)
            or len(set(ports)) != len(ports)):
        raise VMError('invalid_guest_ports')
    if body.get('delete_data') and body.get('confirm_data_loss') is not True:
        raise VMError('confirm_data_loss_required')


class MicroVMs:
    def require_mutable(self, meta, action):
        if meta and action in ('vm/update', 'vm/destroy') and (self.folder(meta)/'.migration-fence').exists():
            raise VMError('migration_source_fenced')

    def execution_allowed(self,meta):
        if (self.folder(meta)/'.migration-fence').exists():return False
        if not self.capacity.allows(meta):return False
        if not self.runtime:return True
        if meta.get('tier')=='anonymous':
            return time.time()<meta.get('anonymous_until',0) and self.runtime.check_policy(meta,force=True)
        ledger=getattr(self.runtime,'ledger',None)
        return self.runtime.check_policy(meta,force=True) and (ledger is None or ledger.lease_allowed(meta['project_id']))
    def __init__(self, config, execute_guest):
        self.root = Path(config['state_dir']).resolve()
        self.images = Path(config['image_dir']).resolve()
        self.debian_images = Path(config['debian_image_dir']).resolve() if config.get('debian_image_dir') else None
        self.debian_images_v2 = Path(config['debian_image_v2_dir']).resolve() if config.get('debian_image_v2_dir') else None
        self.free_images = Path(config['free_image_dir']).resolve() if config.get('free_image_dir') else None
        self.free_images_v3 = Path(config['free_image_v3_dir']).resolve() if config.get('free_image_v3_dir') else None
        self.free_images_v4 = Path(config['free_image_v4_dir']).resolve() if config.get('free_image_v4_dir') else None
        self.free_images_v1 = Path(config['free_image_v1_dir']).resolve() if config.get('free_image_v1_dir') else self.free_images
        from disk_crypto import DiskCrypto
        from seed_crypto import SeedCrypto
        self.disk_crypto = DiskCrypto(config.get('disk_keyring'))
        self.seed_crypto = SeedCrypto(self.disk_crypto)
        if self.disk_crypto.path and self.disk_crypto.path.resolve().is_relative_to(self.root):
            raise VMError('disk_keyring_must_be_outside_vm_storage')
        # QEMU option strings and UNIX socket paths must stay unambiguous.
        if not re.fullmatch(r'/[A-Za-z0-9_./-]+', str(self.root)) or len(str(self.root)) > 45:
            raise VMError('hypervisor_state_dir_must_be_short_absolute_safe_path')
        if not re.fullmatch(r'/[A-Za-z0-9_./-]+', str(self.images)):
            raise VMError('invalid_image_dir')
        if self.free_images and not re.fullmatch(r'/[A-Za-z0-9_./-]+',str(self.free_images)):
            raise VMError('invalid_free_image_dir')
        if self.free_images_v3 and not re.fullmatch(r'/[A-Za-z0-9_./-]+',str(self.free_images_v3)):
            raise VMError('invalid_free_image_v3_dir')
        if self.free_images_v4 and not re.fullmatch(r'/[A-Za-z0-9_./-]+',str(self.free_images_v4)):
            raise VMError('invalid_free_image_v4_dir')
        if self.debian_images and not re.fullmatch(r'/[A-Za-z0-9_./-]+',str(self.debian_images)):
            raise VMError('invalid_debian_image_dir')
        if self.debian_images_v2 and not re.fullmatch(r'/[A-Za-z0-9_./-]+',str(self.debian_images_v2)):
            raise VMError('invalid_debian_image_v2_dir')
        if self.free_images_v1 and not re.fullmatch(r'/[A-Za-z0-9_./-]+',str(self.free_images_v1)):
            raise VMError('invalid_free_image_v1_dir')
        self.execute_guest = execute_guest
        self.diagnostic_serial = config.get('diagnostic_serial', False)
        self.children = {}
        self.qmp_locks = {}
        self.qmp_locks_guard = threading.Lock()
        # Guest base images are mounted read-only in the worker. Hash each
        # version once per worker lifetime, then invalidate on any file change.
        self.image_verify_lock = threading.Lock()
        self.verified_images = {}
        self.ingress_origin = ''
        self.quota_provider = lambda project, owner: {"vcpus": 2, "memory_mib": 4096, "max_vms": 1}
        self.approval_provider = lambda project, owner: {'quota':self.quota_provider(project,owner),
                                                          'network_restricted':False,'tier':'approved'}
        self.runtime = None
        self.cpu_quota_socket = config.get('cpu_quota_socket')
        self.meters = {}
        self.network = None
        if config.get('public_network'):
            from network import Network
            self.network = Network(self, config['public_network'])
        for folder in ('catalog', 'vms', 'retained', 'locks'):
            (self.root / folder).mkdir(parents=True, exist_ok=True, mode=0o700)
        self.prepare_seed_storage()
        from fleet_capacity import Capacity
        self.capacity=Capacity(self)

    def prepare_seed_storage(self):
        for path in sorted((self.root / 'catalog').glob('*.json')):
            meta = json.loads(path.read_text())
            folder = self.folder(meta)
            if not folder.is_dir() and meta.get('retained'):
                folder = self.root / 'retained' / meta['vm_id']
                if folder.is_symlink() or folder.resolve().parent != self.root / 'retained':
                    raise VMError('unsafe_vm_path')
            if folder.is_dir() and meta.get('disk_encryption'):
                self.seed_crypto.protect(meta, folder)

    @contextmanager
    def lock(self, project):
        if not re.fullmatch(r'prj_[0-9a-f]{24}', project):
            raise VMError('invalid_project')
        with (self.root / 'locks' / project).open('a') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise VMError('vm_operation_in_progress')
            yield

    @contextmanager
    def allocation_lock(self):
        with (self.root / 'locks' / 'port-allocation').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            yield

    def reserved_ports(self):
        occupied=set()
        for path in (self.root/'catalog').glob('*.json'):
            other=json.loads(path.read_text())
            if other['state'] in ('destroyed','migrated'): continue
            occupied.add(other['ssh_port'])
            if other.get('proxy_port'):occupied.add(other['proxy_port'])
            occupied.update(p['worker_port'] for p in other.get('ports',[]))
            occupied.update(other.get('public_targets',{}).values())
            occupied.update(other.get('public_ports',[]))
        return occupied

    def reserved_port(self, used=()):
        # Called under allocation_lock, including hibernated unbound endpoints.
        occupied=self.reserved_ports()|set(used)
        for _ in range(1000):
            port=free_port()
            if port not in occupied and not (self.network and self.network.first <= port <= self.network.last): return port
        raise VMError('internal_port_pool_exhausted')

    def catalog(self, project):
        if not re.fullmatch(r'(?:prj_[0-9a-f]{24}|vm_[0-9a-f]{32})', project):
            raise VMError('invalid_project')
        return self.root / 'catalog' / (project + '.json')

    def read(self, project, owner, vm_id=None):
        if not re.fullmatch(r'prj_[0-9a-f]{24}', project): raise VMError('invalid_project')
        if vm_id is not None and not re.fullmatch(r'vm_[0-9a-f]{32}',vm_id): raise VMError('invalid_vm_identity')
        path = self.catalog(vm_id or project)
        if vm_id and not path.exists():
            path = self.catalog(project)  # legacy/default VM retains its original path
        if not path.exists(): return None
        meta = json.loads(path.read_text())
        if meta['project_id'] != project: raise VMError('vm_project_mismatch')
        if meta['owner_did'] != owner: raise VMError('vm_owner_mismatch')
        if vm_id and meta['vm_id'] != vm_id: raise VMError('vm_generation_mismatch')
        return meta

    def list(self, project, owner):
        self.catalog(project)
        result=[]
        for path in (self.root/'catalog').glob('*.json'):
            meta=json.loads(path.read_text())
            if meta['project_id']==project:
                if meta['owner_did']!=owner: raise VMError('vm_owner_mismatch')
                result.append(meta)
        return sorted(result,key=lambda m:m['vm_id'])

    def folder(self, meta):
        vm_id = meta['vm_id']
        if not re.fullmatch(r'vm_[0-9a-f]{32}', vm_id):
            raise VMError('invalid_vm_identity')
        path = self.root / 'vms' / vm_id
        if path.is_symlink() or path.resolve().parent != self.root / 'vms':
            raise VMError('unsafe_vm_path')
        return path

    def save(self, meta):
        key=meta.get('catalog_key',meta['project_id'])
        if key not in (meta['project_id'],meta['vm_id']): raise VMError('invalid_catalog_key')
        atomic_json(self.catalog(key), meta)

    def qmp(self, meta, command, arguments=None):
        if command not in ('query-status', 'quit', 'human-monitor-command', 'stop', 'cont', 'migrate', 'query-migrate', 'migrate_cancel', 'migrate-set-capabilities', 'migrate-incoming'):
            raise VMError('invalid_qmp_command')
        # QEMU's monitor accepts one connection at a time. Keep this mutex
        # separate from the lifecycle lock held by long guest deployments.
        with self.qmp_locks_guard:
            lock = self.qmp_locks.setdefault(meta['vm_id'], threading.Lock())
        if not lock.acquire(timeout=1 if command == 'stop' else 125):
            raise VMError('qmp_monitor_busy')
        try:
            return self._qmp(meta, command, arguments)
        finally:
            lock.release()

    def _qmp(self, meta, command, arguments=None):
        with socket.socket(socket.AF_UNIX) as sock:
            # Restored guests can briefly keep the monitor busy while QEMU
            # finalizes incoming RAM. Lifecycle commands must outlive that
            # transition; readiness polling itself remains short and retryable.
            timeout=120 if command=='human-monitor-command' else 30 if command in ('cont','stop','quit','migrate','migrate_cancel') else 3
            sock.settimeout(timeout)
            sock.connect(str(self.folder(meta) / 'qmp.sock'))
            with sock.makefile('rwb') as io:
                def read():
                    raw = io.readline(65537)
                    if not raw or len(raw) > 65536:
                        raise VMError('invalid_qmp_response')
                    return json.loads(raw)
                if 'QMP' not in read():
                    raise VMError('invalid_qmp_greeting')
                def call(name, identity, args=None):
                    message = {'execute': name, 'id': identity}
                    if args is not None:
                        message['arguments'] = args
                    io.write((json.dumps(message) + '\n').encode())
                    io.flush()
                    for _ in range(64):
                        response = read()
                        if response.get('id') == identity:
                            if 'error' in response:
                                print('GAP_QMP_ERROR '+json.dumps({'command':name,'error':response['error']}),flush=True)
                                raise VMError('qmp_command_failed')
                            return response['return']
                    raise VMError('qmp_event_overflow')
                call('qmp_capabilities', 1)
                if call('query-name', 2).get('name') != meta['vm_id']:
                    raise VMError('qmp_vm_identity_mismatch')
                return call(command, 3, arguments)

    def alive(self, meta):
        # This probe is only for deciding whether deletion/start is safe.
        # Never signal a PID: PIDs can be reused. Mutation uses verified QMP.
        pidfile = self.folder(meta) / 'qemu.pid'
        if not pidfile.exists():
            return False
        try:
            pid = int(pidfile.read_text())
            raw = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0')
            return meta['vm_id'].encode() in raw and str(self.folder(meta) / 'disk.qcow2').encode() in b'\0'.join(raw)
        except (FileNotFoundError, ProcessLookupError):
            return False
        except (ValueError, PermissionError):
            raise VMError('cannot_establish_vm_process_identity')

    def public(self, meta):
        if not meta:
            return {'state': 'absent'}
        result = {key: meta[key] for key in ('vm_id', 'project_id', 'state', 'vcpus', 'memory_mib', 'disk_gib', 'ports')}
        result['tier']=meta.get('tier','approved')
        result['network_policy']=('web_egress' if self.direct_free_egress(meta) else 'reverse_proxy_only') if meta.get('network_restricted') else 'standard'
        if meta['state'] not in ('destroyed', 'creating', 'hibernated', 'hibernating', 'resuming', 'migrated'):
            try:
                result['state'] = self.qmp(meta, 'query-status')['status']
            except (OSError, VMError):
                result['state'] = 'unknown' if self.alive(meta) else 'stopped'
        result['disk_encryption'] = {'enabled':bool(meta.get('disk_encryption')), 'cipher':'AES-256-XTS' if meta.get('disk_encryption') else None}
        result['public_ports'] = meta.get('public_ports', [])
        if self.network:
            result['public_hostname'] = self.network.host
        result['environment_sync_pending'] = meta.get('environment_sync_pending', False)
        if meta.get('retained'):
            result['retained_volume_id'] = meta['vm_id']
        return result

    def guest(self, project, owner, vm_id=None):
        meta = self.read(project, owner, vm_id)
        if not meta or self.public(meta)['state'] != 'running':
            raise VMError('vm_not_running')
        folder = self.folder(meta)
        return {'vm_id': meta['vm_id'], 'address': '127.0.0.1', 'port': meta['ssh_port'],
                'ssh_key': str(folder / 'client_key'), 'known_hosts': str(folder / 'known_hosts')}

    def fence_for_migration(self, project, owner, vm_id, transfer_id):
        if not re.fullmatch(r'move_[0-9a-f]{32}',transfer_id):raise VMError('invalid_transfer_id')
        with self.owner_lock(owner):
            meta=self.read(project,owner,vm_id)
            if not meta or meta['state']=='destroyed':raise VMError('migration_vm_missing')
            fence=self.folder(meta)/'.migration-fence'
            if fence.exists():
                if json.loads(fence.read_text()).get('transfer_id')!=transfer_id:raise VMError('migration_already_fenced')
            else:atomic_json(fence,dict(transfer_id=transfer_id,vm_id=vm_id))
            # Fence first. A failed shutdown never permits a target start.
            self.stop(meta,False)
            if self.alive(meta):raise VMError('migration_source_still_alive')
            return dict(transfer_id=transfer_id,vm_id=vm_id,source_stopped=True)

    def image_dir(self,meta):
        if meta.get('guest_image')=='debian-v2':
            if not self.debian_images_v2:raise VMError('debian_guest_image_unavailable')
            return self.debian_images_v2
        if meta.get('guest_image')=='debian-v1':
            if not self.debian_images:raise VMError('debian_guest_image_unavailable')
            return self.debian_images
        if meta.get('guest_image')=='free-vm-v1':
            if not self.free_images_v1:raise VMError('free_guest_image_unavailable')
            return self.free_images_v1
        if meta.get('guest_image')=='free-vm-v2':
            if not self.free_images:raise VMError('free_guest_image_unavailable')
            return self.free_images
        if meta.get('guest_image')=='free-vm-v3':
            if not self.free_images_v3:raise VMError('free_guest_image_unavailable')
            return self.free_images_v3
        if meta.get('guest_image')=='free-vm-v4':
            if not self.free_images_v4:raise VMError('free_guest_image_unavailable')
            return self.free_images_v4
        return self.images

    @staticmethod
    def direct_free_egress(meta):
        # An existing encrypted v2 overlay cannot be switched to a different
        # backing file in place. Operators may opt one stopped VM into the
        # v3 network path while preserving its original backing image.
        return meta.get('guest_image') in ('free-vm-v3','free-vm-v4') or (meta.get('guest_image')=='free-vm-v2' and meta.get('direct_egress') is True)

    def image_identity(self,meta=None):
        image_dir=self.image_dir(meta or {})
        manifest = (image_dir / 'SHA256SUMS').read_text()
        seen = set()
        assets=[]
        for line in manifest.splitlines():
            parts=line.split()
            if len(parts)!=2: raise VMError('invalid_guest_image_manifest')
            digest, name = parts
            if name not in ('vmlinuz', 'initramfs', 'rootfs.ext4') or name in seen:
                raise VMError('invalid_guest_image_manifest')
            seen.add(name)
            path=image_dir / name
            stat=path.stat()
            assets.append([name,stat.st_dev,stat.st_ino,stat.st_size,stat.st_mtime_ns,stat.st_ctime_ns])
        if seen != {'vmlinuz', 'initramfs', 'rootfs.ext4'}:
            raise VMError('invalid_guest_image_manifest')
        return {'manifest':manifest,'assets':assets}

    def image_version(self,meta=None):
        image_dir=self.image_dir(meta or {})
        identity=self.image_identity(meta)
        manifest=identity['manifest']
        fingerprint=json.dumps(identity,sort_keys=True)
        with self.image_verify_lock:
            cached=self.verified_images.get(str(image_dir))
            if cached and cached[0]==fingerprint:return cached[1]
            for line in manifest.splitlines():
                digest,name=line.split()
                path=image_dir/name
                hasher=hashlib.sha256()
                with path.open('rb') as source:
                    for chunk in iter(lambda: source.read(1024 * 1024), b''):
                        hasher.update(chunk)
                if hasher.hexdigest()!=digest:
                    raise VMError('guest_image_checksum_mismatch')
            version=hashlib.sha256(manifest.encode()).hexdigest()
            self.verified_images[str(image_dir)]=(fingerprint,version)
            return version

    def prepare(self, meta):
        folder = self.folder(meta)
        folder.mkdir(mode=0o700)
        image_dir=self.image_dir(meta)
        meta['image_version'] = self.image_version(meta)
        meta['image_identity'] = self.image_identity(meta)
        self.disk_crypto.initialize(meta)
        if self.disk_crypto.path:
            self.disk_crypto.execute(meta, 'create', folder / 'disk.qcow2', size=str(meta['disk_gib'])+'G', base=image_dir / 'rootfs.ext4')
        else:
            run(['qemu-img', 'create', '-f', 'qcow2', '-F', 'raw', '-b', str(image_dir / 'rootfs.ext4'),str(folder/'disk.qcow2'),str(meta['disk_gib'])+'G'])
        seed = folder / 'seed'
        seed.mkdir(mode=0o700)
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(folder / 'client_key')])
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(seed / 'ssh_host_ed25519_key')])
        (seed / 'authorized_keys').write_text(self.authorized_keys(meta, meta.get('ssh_keys', [])))
        (folder / 'known_hosts').write_text(meta['vm_id'] + ' ' + (seed / 'ssh_host_ed25519_key.pub').read_text())
        (seed / 'runtime.json').write_text(json.dumps(self.environment(meta)))
        self.seed_crypto.rebuild(meta, folder)
        # Seed contains this guest's identity, never the client's private key.

    def environment(self, meta):
        from environment import variables
        return variables(meta, self.network.host if self.network else '', meta.get('ingress_origin',self.ingress_origin))

    def sync_environment(self, meta):
        values = self.environment(meta)
        meta['environment_sync_pending'] = True
        self.save(meta)
        (self.folder(meta) / 'seed' / 'runtime.json').write_text(json.dumps(values))
        if self.alive(meta):
            result = self.execute_guest(self.guest(meta['project_id'], meta['owner_did'], meta['vm_id']),
                {'action': 'runtime_environment', 'body': {'variables': values}}, timeout=15)
            if result.get('ok') is not True:
                raise VMError('guest_environment_update_failed')
        meta['environment_sync_pending'] = False
        self.save(meta)

    def refresh_seed(self, meta):
        folder = self.folder(meta)
        (folder / 'seed' / 'authorized_keys').write_text(self.authorized_keys(meta, meta.get('ssh_keys', [])))
        (folder / 'seed' / 'runtime.json').write_text(json.dumps(self.environment(meta)))
        self.seed_crypto.rebuild(meta, folder)

    def authorized_keys(self, meta, keys):
        terminal_key = self.folder(meta) / 'terminal_key.pub'
        terminal_options=('restrict,pty ' if meta.get('tier')=='anonymous'
                          else ('restrict,pty,command="/bin/bash --login -i" '
                                if meta.get('guest_image') in ('free-vm-v2','free-vm-v3','free-vm-v4','debian-v1','debian-v2')
                                else 'restrict,pty,command="/bin/sh -l" '))
        terminal = (terminal_options + terminal_key.read_text().strip() + '\n') if terminal_key.exists() else ''
        owner_options = 'restrict,pty ' if meta.get('network_restricted') else 'no-agent-forwarding,no-X11-forwarding '
        return ('restrict,command="python3 /usr/local/lib/gap-compose-guest.py" ' +
                (self.folder(meta) / 'client_key.pub').read_text().strip() + '\n' +
                terminal + ''.join(owner_options + key + '\n' for key in keys))

    def authorized_keys_digest(self, meta, keys):
        return hashlib.sha256(self.authorized_keys(meta, keys).encode()).hexdigest()

    def write_keys(self, meta, keys):
        folder = self.folder(meta)
        content = self.authorized_keys(meta, keys)
        # Prepare a replacement seed; never modify a mounted seed image in place.
        (folder / 'seed' / 'authorized_keys').write_text(content)
        if self.alive(meta):
            result = self.execute_guest(self.guest(meta['project_id'], meta['owner_did'], meta['vm_id']),
                                        {'action': 'ssh_keys', 'body': {'content': content}}, timeout=15)
            if result.get('ok') is not True:
                raise VMError('ssh_keys_update_failed')
            meta['authorized_keys_sha256'] = hashlib.sha256(content.encode()).hexdigest()
            self.save(meta)
        self.seed_crypto.rebuild(meta, folder)

    def fast_snapshot(self, meta):
        if meta.get('snapshot_format') == 'gapmem1': return False
        if meta.get('snapshot_format') == 'gapfast1': return True
        if meta.get('state') in ('running', 'paused', 'hibernating') and 'running_fast_snapshot' in meta:
            return meta['running_fast_snapshot'] is True
        selected = set(filter(None, os.environ.get('GAP_FAST_SNAPSHOT_VM_IDS', '').split(',')))
        return os.environ.get('GAP_FAST_SNAPSHOT_ENABLED') == '1' or meta['vm_id'] in selected

    def qemu_binary(self, meta):
        if self.fast_snapshot(meta):
            binary = '/usr/local/bin/qemu-system-x86_64-fast'
            if not os.path.isfile(binary): raise VMError('fast_snapshot_qemu_unavailable')
            return binary
        return 'qemu-system-x86_64'

    def qemu_version(self, meta):
        return subprocess.check_output([self.qemu_binary(meta), '--version'], text=True).splitlines()[0]

    def command(self, meta, seed_path=None):
        folder = self.folder(meta)
        image_dir=self.image_dir(meta)
        forwards = [f'hostfwd=tcp:127.0.0.1:{meta["ssh_port"]}-:22']
        forwards += [f'hostfwd=tcp:127.0.0.1:{port["worker_port"]}-:{port["guest_port"]}' for port in meta['ports']]
        if self.network:
            forwards += ['hostfwd=' + rule for rule in self.network.rules(meta)]
        if meta.get('guest_image') in ('free-vm-v1','free-vm-v2') and not self.direct_free_egress(meta):
            if not meta.get('network_restricted') or not meta.get('proxy_port'):
                raise VMError('free_vm_egress_policy_unavailable')
            # A -tcp chardev is shared for QEMU's whole lifetime, which corrupts
            # the second HTTP request. -cmd spawns a fixed connector per guest
            # TCP connection instead.
            forwards.append(f'guestfwd=tcp:10.0.2.100:3128-cmd:/usr/bin/nc 127.0.0.1 {meta["proxy_port"]}')
        if self.direct_free_egress(meta) and meta.get('network_restricted') and not self.cpu_quota_socket:
            raise VMError('free_vm_host_egress_policy_unavailable')
        command = [self.qemu_binary(meta)] + (['-L', '/usr/share/qemu'] if self.fast_snapshot(meta) else []) + [
                '-machine', 'microvm,accel=kvm', '-cpu', 'host',
                '-name', meta['vm_id'], '-m', str(meta['memory_mib']), '-smp', str(math.ceil(meta['vcpus'])),
                '-kernel', str(image_dir / 'vmlinuz'), '-initrd', str(image_dir / 'initramfs'),
                '-append', 'console=ttyS0 root=/dev/vda rootfstype=ext4 modules=virtio_mmio,virtio_blk,ext4 rootwait rw reboot=t net.ifnames=0',
                '-nodefaults', '-no-user-config', '-display', 'none',
                '-serial', f'file:{folder}/serial.log' if self.diagnostic_serial else 'null', '-no-reboot',
                '-sandbox', ('on,obsolete=deny,spawn=allow,resourcecontrol=deny' if meta.get('guest_image') in ('free-vm-v1','free-vm-v2') and not self.direct_free_egress(meta)
                             else 'on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny'),
                '-drive', f'id=root,file={folder}/disk.qcow2,format=qcow2,if=none'+(',encrypt.key-secret=gapdisk' if meta.get('disk_encryption') else ''),
                '-device', 'virtio-blk-device,drive=root',
                '-drive', f'id=seed,file={seed_path or folder / "seed.ext4"},format=raw,if=none,readonly=on',
                '-device', 'virtio-blk-device,drive=seed',
                '-device', 'virtio-rng-device',
                '-netdev', 'user,id=net0,' + ('restrict=on,' if meta.get('network_restricted') and not self.direct_free_egress(meta) else '') + ('ipv6=off,' if self.direct_free_egress(meta) else '') + ','.join(forwards), '-device', 'virtio-net-device,netdev=net0,mac=' + self.guest_mac(meta),
                '-qmp', f'unix:{folder}/qmp.sock,server=on,wait=off',
                '-pidfile', str(folder / 'qemu.pid')]
        if self.runtime:
            command += ['-object', f'filter-dump,id=meterin,netdev=net0,queue=tx,file={folder}/meter-in.fifo,maxlen=96',
                        '-object', f'filter-dump,id=meterout,netdev=net0,queue=rx,file={folder}/meter-out.fifo,maxlen=96']
        return command

    def enforce_cpu(self, meta, process, quota_vcpus=None, quota_only=False):
        quota_vcpus = meta['vcpus'] if quota_vcpus is None else quota_vcpus
        if not self.cpu_quota_socket:
            if self.direct_free_egress(meta) and meta.get('network_restricted'):
                raise VMError('free_vm_host_egress_policy_unavailable')
            if meta['vcpus'] != int(meta['vcpus']):
                raise VMError('fractional_cpu_requires_quota_broker')
            return
        request={'pid':process.pid,'vm_id':meta['vm_id'],'vcpus':quota_vcpus}
        if quota_only: request['quota_only']=True
        elif self.direct_free_egress(meta):
            request['egress_policy']='free_web_v1' if meta.get('network_restricted') else 'standard'
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(5)
            sock.connect(self.cpu_quota_socket)
            sock.sendall((json.dumps(request)+'\n').encode())
            response = sock.makefile('rb').readline(2049)
        expected = int(quota_vcpus*100000)
        result = json.loads(response)
        if result != {'ok':True,'quota_us':expected,'period_us':100000}:
            raise VMError('cpu_quota_unavailable')

    @staticmethod
    def guest_mac(meta):
        return '52:54:' + ':'.join(meta['vm_id'][3+i:5+i] for i in (0,2,4,6))

    def capture_start(self, meta):
        if self.runtime:
            from meter import PacketMeter
            self.capture_stop(meta)
            self.meters[meta['vm_id']] = PacketMeter(self.folder(meta), self.guest_mac(meta))

    def capture_stop(self, meta):
        meter = self.meters.pop(meta['vm_id'], None)
        if meter: meter.close()

    def hibernate(self, meta):
        if meta['state'] == 'hibernated': return
        if self.public(meta)['state'] not in ('running','paused'):
            raise VMError('hibernate_requires_running_vm')
        if shutil.disk_usage(self.folder(meta)).free < meta['memory_mib']*1024**2+512*1024**2:
            raise VMError('insufficient_disk_for_hibernation')
        current_identity=self.image_identity(meta)
        if meta.get('image_identity') != current_identity:
            if self.image_version(meta)!=meta['image_version']: raise VMError('guest_base_image_changed')
            meta['image_identity']=current_identity
        meta['snapshot_qemu_version']=self.qemu_version(meta)
        tag='idle_' + uuid.uuid4().hex
        # Record which keys are already present in the memory image. The usual
        # wake can then skip an SSH round trip while still applying key changes
        # made during hibernation before application traffic is released.
        meta['authorized_keys_sha256'] = self.authorized_keys_digest(meta, meta.get('ssh_keys', []))
        meta.update(state='hibernating', snapshot_tag=tag)
        if self.runtime:self.runtime.execution_stopping(meta)
        self.save(meta)
        try:
            self.qmp(meta,'stop')
            if self.fast_snapshot(meta):
                from memory_fast import save
                save(self,meta)
                meta['snapshot_format']='gapfast1'
                self.save(meta)
            elif meta.get('disk_encryption'):
                from memory_crypto import save
                save(self,meta)
                meta['snapshot_format']='gapmem1'
                self.save(meta)
            else:
                response=self.qmp(meta,'human-monitor-command',{'command-line':'savevm '+tag})
                if response.strip(): raise VMError('snapshot_save_failed')
            self.qmp(meta,'quit')
            child=self.children.pop(meta['vm_id'],None)
            if child: child.wait(timeout=15)
            deadline=time.monotonic()+15
            while self.alive(meta) and time.monotonic()<deadline: time.sleep(.05)
            if self.alive(meta): raise VMError('hibernate_process_still_alive')
            self.capture_stop(meta)
            meta.update(state='hibernated',hibernated_at=int(time.time()))
            self.save(meta)
        except Exception:
            if self.alive(meta):
                if not self.execution_allowed(meta):
                    self.stop(meta,True)
                else:
                    self.qmp(meta,'cont')
                    if self.runtime:self.runtime.execution_started(meta)
                    meta['state']='running'; self.save(meta)
            raise

    def resume(self, meta):
        profile_started=time.monotonic()
        self.disk_crypto.key(meta)
        key_ready=time.monotonic()
        if meta['state'] != 'hibernated':
            return self.start(meta)
        tag=meta.get('snapshot_tag','')
        if not re.fullmatch(r'idle_[0-9a-f]{32}',tag): raise VMError('invalid_snapshot_identity')
        current_identity=self.image_identity(meta)
        if meta.get('image_identity') != current_identity:
            # Legacy snapshots get one full verification; subsequent wakes use
            # the persisted identity even after the worker restarts.
            if self.image_version(meta)!=meta['image_version']: raise VMError('guest_base_image_changed')
            meta['image_identity']=current_identity
            self.save(meta)
        if meta.get('snapshot_qemu_version')!=self.qemu_version(meta):
            raise VMError('snapshot_requires_original_qemu_version')
        self.ensure_free_vm_proxy(meta)
        meta['running_fast_snapshot'] = meta.get('snapshot_format') == 'gapfast1'
        meta['state']='resuming'; self.save(meta)
        self.capture_start(meta)
        if meta.get('snapshot_format')=='gapfast1':
            from memory_fast import restore
            return restore(self,meta)
        if meta.get('disk_encryption'):
            print('GAP_RESUME_PREP_PROFILE '+json.dumps({'vm_id':meta['vm_id'],
                'key_ms':round((key_ready-profile_started)*1000),
                'other_ms':round((time.monotonic()-key_ready)*1000)}),flush=True)
            return self.resume_encrypted(meta)
        folder=self.folder(meta);log=folder/'restore.log'
        with log.open('wb') as error, self.disk_crypto.secret(meta) as (secret, fds), self.seed_crypto.image(meta, folder) as (seed_path, seed_fds):
            process=subprocess.Popen(self.command(meta,seed_path)+[x.replace('--object','-object') for x in secret]+['-loadvm',tag,'-S'],pass_fds=fds+seed_fds,stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,stderr=error,env={'PATH':'/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin'})
        self.children[meta['vm_id']]=process
        deadline=time.monotonic()+90
        try:
            self.enforce_cpu(meta,process)
            while time.monotonic()<deadline:
                if process.poll() is not None: raise VMError('snapshot_restore_failed')
                try:
                    if self.qmp(meta,'query-status')['status'] in ('paused','prelaunch'): break
                except OSError: pass
                time.sleep(.01)
            else: raise VMError('snapshot_restore_timeout')
            # Mark resumed before executing guest instructions: never replay an old
            # memory snapshot after a crash following an externally visible write.
            meta['state']='running'; self.save(meta)
            if not self.execution_allowed(meta):raise VMError('microvm_suspended_or_policy_unavailable')
            self.qmp(meta,'cont')
            if self.runtime: self.runtime.execution_started(meta)
            response=self.qmp(meta,'human-monitor-command',{'command-line':'delvm '+tag})
            if response.strip(): meta['snapshot_cleanup_pending']=True
            else: meta.pop('snapshot_tag',None)
            self.save(meta)
        except Exception:
            if process.poll() is None:
                process.terminate(); process.wait(timeout=10)
            self.capture_stop(meta)
            if meta['state']=='resuming': meta['state']='hibernated'
            else: meta['state']='stopped'
            self.save(meta)
            raise

    def resume_encrypted(self,meta):
        from memory_crypto import restore
        profile_started=time.monotonic()
        folder=self.folder(meta)
        if meta.get('snapshot_format')!='gapmem1':
            meta['state']='hibernated';self.save(meta)
            raise VMError('snapshot_format_invalid')
        (folder/'memory.sock').unlink(missing_ok=True)
        process=None
        try:
            with (folder/'restore.log').open('wb') as log, self.disk_crypto.secret(meta) as (secret,fds), self.seed_crypto.image(meta, folder) as (seed_path,seed_fds):
                process=subprocess.Popen(self.command(meta,seed_path)+[x.replace('--object','-object') for x in secret]+['-incoming','unix:'+str(folder/'memory.sock'),'-S'],
                    pass_fds=fds+seed_fds,stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=log,
                    env={'PATH':'/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin'})
            self.children[meta['vm_id']]=process
            self.enforce_cpu(meta,process)
            launched=time.monotonic()
            restore(self,meta,process)
            restored=time.monotonic()
            deadline=time.monotonic()+90
            while time.monotonic()<deadline:
                if process.poll() is not None:raise VMError('snapshot_restore_failed')
                try:
                    if self.qmp(meta,'query-status')['status']=='paused':break
                except OSError:pass
                time.sleep(.02)
            else:raise VMError('snapshot_restore_timeout')
            if not self.execution_allowed(meta):raise VMError('microvm_suspended_or_policy_unavailable')
            checked=time.monotonic()
            meta['state']='running';self.save(meta)
            catalog_saved=time.monotonic()
            self.qmp(meta,'cont')
            continued=time.monotonic()
            if self.runtime:self.runtime.execution_started(meta)
            (folder/'memory.enc').unlink()
            meta.pop('snapshot_tag',None);meta.pop('snapshot_format',None);meta.pop('resume_error',None);self.save(meta)
            print('GAP_RESTORE_PROFILE '+json.dumps({'vm_id':meta['vm_id'],
                'launch_ms':round((launched-profile_started)*1000),
                'stream_ms':round((restored-launched)*1000),
                'qmp_and_policy_ms':round((checked-restored)*1000),
                'catalog_save_ms':round((catalog_saved-checked)*1000),
                'cont_ms':round((continued-catalog_saved)*1000),
                'cleanup_ms':round((time.monotonic()-continued)*1000)}),flush=True)
        except Exception as error:
            if process and process.poll() is None:process.terminate();process.wait(timeout=10)
            self.capture_stop(meta)
            meta['resume_error'] = str(error) if isinstance(error, VMError) else type(error).__name__
            if meta['state']=='resuming':meta['state']='hibernated'
            else:meta['state']='stopped'
            self.save(meta)
            raise

    def ensure_free_vm_proxy(self, meta):
        if meta.get('guest_image') in ('free-vm-v1','free-vm-v2') and not self.direct_free_egress(meta):
            manager=getattr(self,'free_vm_proxy',None)
            if manager is None:raise VMError('free_vm_egress_proxy_unavailable')
            manager.ensure(meta['vm_id'],meta['proxy_port'])

    def release_free_vm_egress(self, meta):
        if not self.direct_free_egress(meta) or not self.cpu_quota_socket:
            return
        with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as sock:
            sock.settimeout(5)
            sock.connect(self.cpu_quota_socket)
            sock.sendall((json.dumps({'action':'release_egress','vm_id':meta['vm_id']})+'\n').encode())
            response=sock.makefile('rb').readline(2049)
        if json.loads(response)!={'ok':True}:
            raise VMError('free_vm_egress_cleanup_unavailable')

    def start(self, meta):
        self.disk_crypto.key(meta)
        if not self.execution_allowed(meta):raise VMError('microvm_suspended_or_policy_unavailable')
        self.ensure_free_vm_proxy(meta)
        if meta['state'] == 'creating':
            raise VMError('vm_creation_incomplete_destroy_and_retry')
        if self.public(meta)['state'] == 'running':
            return
        if self.alive(meta):
            raise VMError('vm_process_alive_but_unresponsive')
        if self.image_version(meta) != meta['image_version']:
            raise VMError('guest_base_image_changed')
        folder = self.folder(meta)
        self.refresh_seed(meta)
        meta['running_fast_snapshot'] = self.fast_snapshot(meta)
        meta['authorized_keys_sha256'] = self.authorized_keys_digest(meta, meta.get('ssh_keys', []))
        self.capture_start(meta)
        error_log = folder / 'hypervisor.log'
        with (error_log.open('wb') if self.diagnostic_serial or self.fast_snapshot(meta) else open(os.devnull, 'wb')) as log, self.disk_crypto.secret(meta) as (secret, fds), self.seed_crypto.image(meta, folder) as (seed_path, seed_fds):
            process = subprocess.Popen(self.command(meta,seed_path)+[x.replace('--object','-object') for x in secret]+['-S'], pass_fds=fds+seed_fds, stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, stderr=log, start_new_session=True,
                env={'PATH': '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin'})
        self.children[meta['vm_id']] = process
        try:
            self.enforce_cpu(meta, process)
        except Exception:
            process.terminate(); process.wait(timeout=10)
            self.capture_stop(meta)
            raise
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if process.poll() is not None:
                error = VMError('hypervisor_start_failed')
                error.diagnostic = error_log.read_text(errors='replace')[-4096:] if error_log.exists() else ''
                raise error
            try:
                if self.qmp(meta, 'query-status')['status'] in ('prelaunch','paused'):
                    if not self.execution_allowed(meta):
                        process.terminate(); process.wait(timeout=10)
                        raise VMError('microvm_suspended_or_policy_unavailable')
                    self.qmp(meta, 'cont')
                    break
            except OSError:
                pass
            time.sleep(.1)
        else:
            process.terminate()
            process.wait(timeout=10)
            raise VMError('hypervisor_start_timeout')
        if not self.execution_allowed(meta):
            self.stop(meta,True)
            raise VMError('microvm_suspended_or_policy_unavailable')
        if self.runtime: self.runtime.execution_started(meta,cold=True)
        meta['state'] = 'running'
        meta['network_pending'] = False
        meta['environment_sync_pending'] = False
        self.save(meta)

    def stop(self, meta, force):
        if not self.alive(meta):
            meta['state'] = 'stopped'
            self.save(meta)
            if meta.get('guest_image') in ('free-vm-v1','free-vm-v2') and getattr(self,'free_vm_proxy',None):
                self.free_vm_proxy.release(meta['vm_id'])
            self.release_free_vm_egress(meta)
            return
        if force:
            try: self.qmp(meta, 'quit')
            except (OSError,VMError):
                # Only signal an unreaped child owned by this worker. Never
                # trust an arbitrary/reused PID read from disk.
                child=self.children.get(meta['vm_id'])
                if child is None or child.poll() is not None: raise
                child.kill(); child.wait(timeout=10)
        else:
            self.execute_guest(self.guest(meta['project_id'], meta['owner_did'], meta['vm_id']),
                               {'action': 'vm_shutdown', 'body': {}}, timeout=15)
        deadline = time.monotonic() + 30
        while self.alive(meta) and time.monotonic() < deadline:
            time.sleep(.1)
        if self.alive(meta):
            raise VMError('vm_still_running_use_explicit_force_stop')
        self.capture_stop(meta)
        meta['state'] = 'stopped'
        self.save(meta)
        child = self.children.pop(meta['vm_id'], None)
        if child:
            child.wait(timeout=5)
        if meta.get('guest_image') in ('free-vm-v1','free-vm-v2') and getattr(self,'free_vm_proxy',None):
            self.free_vm_proxy.release(meta['vm_id'])
        self.release_free_vm_egress(meta)

    @contextmanager
    def owner_lock(self, owner):
        if not re.fullmatch(r'did:gap:[0-9a-f]{64}', owner):
            raise VMError('invalid_owner_identity')
        with (self.root / 'locks' / ('owner-' + owner[8:])).open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            yield

    def quota_usage(self, owner, include_disk=False):
        usage = {'vcpus': 0, 'memory_mib': 0}
        if include_disk: usage['disk_gib']=0
        for path in (self.root / 'catalog').glob('*.json'):
            meta = json.loads(path.read_text())
            if include_disk and meta['owner_did']==owner and meta['state']=='destroyed' and meta.get('retained'):
                usage['disk_gib']+=meta['disk_gib']
            if meta['owner_did'] == owner and meta['state'] not in ('destroyed','migrated'):
                for key in usage:
                    value = meta[key]
                    if key == 'vcpus':
                        from cpu_quota import quarters
                        try: quarters(value)
                        except ValueError: raise VMError('invalid_resource_catalog')
                    elif type(value) is not int or value <= 0:
                        raise VMError('invalid_resource_catalog')
                    usage[key] += value
        return usage

    def vm_count(self, owner):
        return sum(1 for path in (self.root / 'catalog').glob('*.json')
                   if (meta := json.loads(path.read_text()))['owner_did'] == owner
                   and meta['state'] not in ('destroyed','migrated'))

    def quota_view(self, project, owner):
        with self.owner_lock(owner):
            approval=self.approval_provider(project,owner);limits=approval.get('quota')
            return {'limits': limits, 'allocated': dict(self.quota_usage(owner,include_disk=True), max_vms=self.vm_count(owner)),
                    'always_on_allowed': bool(self.runtime and self.runtime.runner.authorize(project,owner).get('always_on_allowed')),
                    'tier':approval.get('tier','approved'),'network_policy':('web_egress' if self.free_images_v4 or self.free_images_v3 else 'reverse_proxy_only') if approval.get('network_restricted') else 'standard',
                    'minimum_disk_gib': max(1,((self.free_images_v4 or self.free_images_v3 or self.free_images if approval.get('tier')=='anonymous' else self.debian_images_v2 or self.debian_images or self.images).joinpath('rootfs.ext4').stat().st_size+1024**3-1)//1024**3)}

    def perform(self, project, owner, action, body):
        validate(action, body)
        if 'vcpus' in body and body['vcpus'] != int(body['vcpus']) and not self.cpu_quota_socket:
            raise VMError('fractional_cpu_requires_quota_broker')
        # All lifecycle changes share an owner lock across projects and processes.
        # Read live approval/quota after acquiring it, before reserving resources.
        with self.owner_lock(owner):
            approval = self.approval_provider(project, owner)
            limits = approval.get('quota') if isinstance(approval,dict) else None
            valid_limits = isinstance(limits, dict) and {'vcpus', 'memory_mib'} <= set(limits) <= {'vcpus', 'memory_mib', 'max_vms', 'disk_gib'}
            if valid_limits:
                from cpu_quota import quarters
                try:
                    quarters(limits['vcpus'])
                except ValueError:
                    valid_limits = False
                valid_limits = valid_limits and all(
                    type(value) is int and 0 < value < 2**31
                    for key, value in limits.items() if key != 'vcpus')
            if not valid_limits:
                raise VMError('invalid_agent_quota')
            meta = None if action=='vm/create' and body.get('new_vm') else self.read(project, owner, body.get('vm_id'))
            network_restricted=approval.get('network_restricted') is True
            default_vcpus=min(1,limits['vcpus']);default_memory_mib=min(1024,limits['memory_mib'])
            if meta and meta.get('network_restricted',False)!=network_restricted:
                if self.alive(meta): raise VMError('network_policy_restart_required')
                meta['network_restricted']=network_restricted
                meta['tier']=approval.get('tier','approved')
                self.save(meta)
            if action in ('vm/create', 'vm/update', 'vm/start', 'vm/resume'):
                usage = self.quota_usage(owner,include_disk=True)
                for key, default in (('vcpus', default_vcpus), ('memory_mib', default_memory_mib), ('disk_gib',8)):
                    if key not in limits:continue
                    if action == 'vm/create':
                        requested, previous = body.get(key, default), 0
                    elif meta and meta['state'] != 'destroyed':
                        previous = meta[key]
                        requested = body.get(key, previous) if action == 'vm/update' else previous
                    else:
                        continue
                    total = usage[key] - previous + requested
                    # After a quota reduction, allow releases and reductions, never growth.
                    if total > limits[key] and (action != 'vm/update' or requested > previous):
                        raise VMError('agent_quota_exceeded_' + key)
            if action=='vm/create':
                if body.get('execution_mode')=='always_on' and not (self.runtime and self.runtime.runner.authorize(project,owner).get('always_on_allowed')):
                    raise VMError('always_on_not_approved')
                image_dir=(self.free_images_v4 or self.free_images_v3 or self.free_images) if approval.get('tier')=='anonymous' and (self.free_images_v4 or self.free_images_v3 or self.free_images) else self.debian_images_v2 or self.debian_images or self.images
                minimum=max(1,((image_dir/'rootfs.ext4').stat().st_size+1024**3-1)//1024**3)
                if body.get('disk_gib',8)<minimum:raise VMError('disk_smaller_than_guest_image')
            if (action == 'vm/create' and (not meta or meta['state'] == 'destroyed')
                    and self.vm_count(owner) >= limits.get('max_vms', 1)):
                raise VMError('agent_quota_exceeded_max_vms')
            self.require_mutable(meta, action)
            if self.capacity.managed(project):
                return self.capacity.execute(project,owner,action,body,approval)
            return self._perform(project, owner, action, body, approval=approval)

    def _perform(self, project, owner, action, body, created_vm_id=None, defer_start=False, approval=None):
        validate(action, body)
        with self.lock(project):
            meta = None if action=='vm/create' and body.get('new_vm') else self.read(project, owner, body.get('vm_id'))
            self.require_mutable(meta, action)
            if action == 'vm/create':
                if meta and meta['state'] != 'destroyed':
                    raise VMError('vm_already_exists')
                approval=approval or self.approval_provider(project,owner)
                limits=approval['quota'];default_vcpus=min(1,limits['vcpus']);default_memory_mib=min(1024,limits['memory_mib'])
                network_restricted=approval.get('network_restricted') is True
                with self.allocation_lock():
                    previous=self.read(project,owner)
                    additional=previous is not None and previous['state']!='destroyed'
                    if previous and previous['state']=='destroyed':
                        # Preserve retained generations and their billing checkpoints.
                        previous['catalog_key']=previous['vm_id']
                        self.catalog(project).replace(self.catalog(previous['vm_id']))
                        self.save(previous)
                    meta = {'vm_id': created_vm_id or 'vm_' + uuid.uuid4().hex, 'project_id': project, 'owner_did': owner,
                            'state': 'creating', 'vcpus': body.get('vcpus', default_vcpus),
                            'execution_mode': body.get('execution_mode','serverless'),
                            'network_restricted': network_restricted, 'tier': approval.get('tier','approved'),
                            **({'guest_image':'free-vm-v4' if self.free_images_v4 else 'free-vm-v3' if self.free_images_v3 else 'free-vm-v2'} if approval.get('tier')=='anonymous' else {}),
                            **({'guest_image':'debian-v2' if self.debian_images_v2 else 'debian-v1'} if approval.get('tier')!='anonymous' and (self.debian_images_v2 or self.debian_images) else {}),
                            **({'anonymous_until':approval['anonymous_until'],'claim_until':approval['claim_until']}
                               if approval.get('tier')=='anonymous' else {}),
                            'memory_mib': body.get('memory_mib', default_memory_mib), 'disk_gib': body.get('disk_gib', 8),
                            'ssh_port': self.reserved_port(), 'ports': [], 'retained': False,
                            'ssh_keys': __import__('network').keys(body.get('ssh_keys', []))}
                    meta['catalog_key']=meta['vm_id'] if additional else project
                    used = {meta['ssh_port']}
                    if meta.get('guest_image') in ('free-vm-v1','free-vm-v2'):
                        meta['proxy_port']=self.reserved_port(used)
                        used.add(meta['proxy_port'])
                    for port in body.get('ports', []):
                        candidate = self.reserved_port(used)
                        while candidate in used:
                            candidate = self.reserved_port(used)
                        used.add(candidate)
                        meta['ports'].append({'guest_port': port, 'worker_port': candidate})
                    if self.network:
                        self.network.allocate(meta)
                    self.save(meta)  # durable reservation before creating disks/processes
                self.prepare(meta)
                meta['state'] = 'stopped'
                self.save(meta)
                if body.get('start', True) and not defer_start:
                    self.start(meta)
            else:
                if not meta or meta['state'] == 'destroyed':
                    raise VMError('vm_not_found')
                if body['vm_id'] != meta['vm_id']:
                    raise VMError('vm_generation_mismatch')
                if action in ('vm/start','vm/resume'):
                    self.resume(meta) if meta['state']=='hibernated' else self.start(meta)
                elif action == 'vm/hibernate':
                    self.hibernate(meta)
                elif action == 'vm/stop':
                    self.stop(meta, body.get('force', False))
                elif action == 'vm/update':
                    if meta['state']=='hibernated': raise VMError('resume_then_stop_before_resize')
                    if self.alive(meta):
                        raise VMError('stop_vm_before_reconfiguration')
                    if body.get('disk_gib', meta['disk_gib']) < meta['disk_gib']:
                        raise VMError('disk_shrink_not_supported')
                    if body.get('disk_gib', meta['disk_gib']) > meta['disk_gib']:
                        self.disk_crypto.execute(meta,'resize',self.folder(meta)/'disk.qcow2',size=str(body['disk_gib'])+'G')
                    for key in ('vcpus', 'memory_mib', 'disk_gib'):
                        meta[key] = body.get(key, meta[key])
                    with self.allocation_lock():
                        if 'ports' in body:
                            used = {meta['ssh_port']}
                            meta['ports'] = []
                            for port in body['ports']:
                                candidate = self.reserved_port(used)
                                while candidate in used:
                                    candidate = self.reserved_port(used)
                                used.add(candidate)
                                meta['ports'].append({'guest_port': port, 'worker_port': candidate})
                        self.save(meta)
                elif action == 'vm/destroy':
                    if self.alive(meta):
                        raise VMError('stop_vm_before_destruction')
                    folder = self.folder(meta)
                    if folder.exists():
                        if body.get('delete_data', False):
                            # Exact generated ID under a validated controller root.
                            shutil.rmtree(folder)
                        else:
                            atomic_json(folder/'retention.json',{'project_id':project,'owner_did':owner,'vm_id':meta['vm_id']})
                            folder.rename(self.root / 'retained' / meta['vm_id'])
                    meta['state'] = 'destroyed'
                    meta['retained'] = not body.get('delete_data', False)
                    self.save(meta)
            return {'ok': True, 'vm': self.public(meta)}
