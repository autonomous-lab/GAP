"""Inbound-idle lifecycle, persistent metering and credit retention policy."""
from contextlib import contextmanager
from collections import deque
import json
import os
from pathlib import Path
import shutil
import threading
import time
import uuid

from billing import Ledger, BillingError
from microvm import VMError


class Runtime:
    def __init__(self,runner,config):
        self.runner=runner; self.manager=runner.hypervisor
        self.manager.runtime=self
        self.ledger=Ledger(Path(config['state_dir'])/'microvm-credits.sqlite')
        self.incarnation=uuid.uuid4().hex
        self.locks={}; self.guard=threading.Lock(); self.states={}
        self.capacity_lock=threading.RLock()
        self.reserve_memory_mib=config.get('host_reserve_memory_mib',2048)
        self.reserve_vcpus=config.get('host_reserve_vcpus',1)
        self.gateway=None; self.closed=False; self.last_error=None
        self.recover()

    def lock(self,project):
        with self.guard: return self.locks.setdefault(project,threading.RLock())

    def state(self,meta):
        return self.states.setdefault(meta['vm_id'],{'last_incoming':meta.get('last_incoming_at',time.time()),
            'active_http':0,'last_sample':0,'connections':set(),
            'guest_clock':meta.get('guest_clock_seconds',0),'running_since':None,
            'tcp_ports':dict(meta.get('tcp_recent_ports',{})),
            'tcp_port_order':deque(sorted(meta.get('tcp_recent_ports',{}).items(),key=lambda item:item[1]))})

    def execution_started(self,meta,cold=False):
        state=self.state(meta)
        if cold:
            state['guest_clock']=0; state['tcp_ports'].clear();state['tcp_port_order'].clear()
            meta.pop('tcp_recent_ports',None);meta.pop('guest_clock_seconds',None)
        state['running_since']=time.monotonic()

    def guest_clock(self,state):
        return state['guest_clock']+(time.monotonic()-state['running_since'] if state['running_since'] is not None else 0)

    def reserve_tcp_source(self,meta,port):
        # Guest TCP TIME_WAIT survives snapshots while the host slirp process
        # loses its matching state. Quarantine by guest ON time, not wall time.
        state=self.state(meta); clock=self.guest_clock(state); cutoff=clock-120
        while state['tcp_port_order'] and state['tcp_port_order'][0][1]<=cutoff:
            old,stamp=state['tcp_port_order'].popleft()
            if state['tcp_ports'].get(old)==stamp:state['tcp_ports'].pop(old,None)
        key=str(port)
        if key in state['tcp_ports']: return False
        state['tcp_ports'][key]=clock;state['tcp_port_order'].append((key,clock))
        return True

    def execution_stopping(self,meta):
        state=self.state(meta);state['guest_clock']=self.guest_clock(state);state['running_since']=None
        meta['guest_clock_seconds']=state['guest_clock']
        meta['tcp_recent_ports']={p:t for p,t in state['tcp_ports'].items() if t>state['guest_clock']-120}

    def recover(self):
        for path in (self.manager.root/'catalog').glob('prj_*.json'):
            meta=json.loads(path.read_text())
            if self.manager.alive(meta):
                # Worker/container restart normally also stops QEMU. Never
                # continue an unmetered orphan left by a different deployment.
                self.manager.qmp(meta,'quit')
                deadline=time.monotonic()+15
                while self.manager.alive(meta) and time.monotonic()<deadline: time.sleep(.05)
                if self.manager.alive(meta): raise VMError('orphan_vm_still_running')
            if meta['state'] in ('hibernating','resuming'):
                import subprocess
                try:
                    disk=json.loads(subprocess.check_output(['qemu-img','info','--output=json',str(self.manager.folder(meta)/'disk.qcow2')]))
                    exists=any(s['name']==meta.get('snapshot_tag') for s in disk.get('snapshots',[]))
                except Exception: exists=False
                meta['state']='hibernated' if exists else 'stopped'
            elif meta['state']=='running': meta['state']='stopped'
            if meta['state']!='destroyed' and meta.get('public_ports') and not meta.get('public_targets'):
                with self.manager.allocation_lock():
                    used={meta['ssh_port'],*(p['worker_port'] for p in meta.get('ports',[]))}
                    meta['public_targets']={}
                    for i in range(1,6):
                        target=self.manager.reserved_port(used);used.add(target)
                        meta['public_targets'][str(i)]=target
                    self.manager.save(meta)
            else: self.manager.save(meta)

    def storage_bytes(self,meta):
        total=0
        folders=[self.manager.folder(meta)]
        for path in (self.manager.root/'retained').glob('vm_*'):
            marker=path/'retention.json'
            if marker.exists():
                who=json.loads(marker.read_text())
                if who.get('project_id')==meta['project_id'] and who.get('owner_did')==meta['owner_did']:
                    folders.append(path)
        for folder in folders:
            if not folder.exists(): continue
            for path in folder.rglob('*'):
                if path.is_file() and not path.is_symlink(): total+=path.stat().st_blocks*512
        return total

    def sample(self,meta,force=True):
        state=self.state(meta)
        if not force and time.monotonic()-state["last_sample"]<5: return
        meter=self.manager.meters.get(meta['vm_id'])
        if meter: incoming,outgoing=meter.values()
        else:
            path=self.manager.folder(meta)/'network-counts.json'
            counters=json.loads(path.read_text()) if path.exists() else {'in':0,'out':0}
            incoming,outgoing=counters['in'],counters['out']
        self.ledger.sample(meta,int(time.time()*1000),self.manager.alive(meta),
                           self.storage_bytes(meta),incoming,outgoing,self.incarnation)
        state['last_sample']=time.monotonic()

    def view(self,meta):
        if not meta: return {}
        state=self.state(meta)
        account=self.ledger.view(meta['project_id'],meta['owner_did'])
        tariff=account['tariff']
        return {'mode':meta.get('execution_mode','serverless'),'idle_timeout_seconds':meta.get('idle_timeout_seconds',900),
                'last_incoming_at':state['last_incoming'],'active_http_requests':state['active_http'],'runtime_error':meta.get('runtime_error'),
                'estimated_on_hour_microcredits':(meta['vcpus']*tariff['vcpu_hour']+
                    meta['memory_mib']*tariff['gib_ram_hour']//1024) if tariff else None,
                'billing':account}

    def check_credit(self,meta):
        view=self.ledger.view(meta['project_id'],meta['owner_did'],include_entries=False)
        if not view['execution_allowed']: raise VMError('microvm_credits_or_budget_exhausted')

    def configure(self,project,owner,body):
        if not {'request_id','vm_id','mode'} <= set(body) <= {'request_id','vm_id','mode','idle_timeout_seconds'} or body['mode'] not in ('serverless','always_on'):
            raise VMError('invalid_runtime_configuration')
        if 'idle_timeout_seconds' in body and (type(body['idle_timeout_seconds']) is not int or not 60<=body['idle_timeout_seconds']<=3600):
            raise VMError('idle_timeout_must_be_60_to_3600_seconds')
        with self.lock(project),self.manager.lock(project):
            meta=self.manager.read(project,owner)
            if not meta or meta['vm_id']!=body['vm_id'] or meta['state']=='destroyed': raise VMError('vm_generation_mismatch')
            approval=self.runner.authorize(project,owner)
            if body['mode']=='always_on' and not approval.get('always_on_allowed'):
                raise VMError('always_on_not_approved')
            self.sample(meta)
            meta['execution_mode']=body['mode']
            if 'idle_timeout_seconds' in body: meta['idle_timeout_seconds']=body['idle_timeout_seconds']
            self.manager.save(meta)
            return {'ok':True,'runtime':self.view(meta)}

    def touch(self,meta):
        self.state(meta)['last_incoming']=time.time()

    @contextmanager
    def admission(self,project,vcpus,memory_mib):
        with self.capacity_lock:
            allocated=0
            for path in (self.manager.root/'catalog').glob('prj_*.json'):
                meta=json.loads(path.read_text())
                if meta['project_id']!=project and self.manager.alive(meta): allocated+=meta['vcpus']
            available=next(int(line.split()[1]) for line in Path('/proc/meminfo').read_text().splitlines() if line.startswith('MemAvailable:'))//1024
            if available<memory_mib+self.reserve_memory_mib or allocated+vcpus>max(1,(os.cpu_count() or 1)-self.reserve_vcpus):
                raise VMError('host_capacity_unavailable_retry_later')
            yield

    def ensure_awake(self,project):
        # Caller holds project lifecycle lock; concurrent first requests share
        # one restore. Recheck owner approval and quotas on every wake.
        path=self.manager.catalog(project)
        if not path.exists(): raise VMError('vm_not_found')
        meta=json.loads(path.read_text())
        if meta['state'] not in ('running','hibernated'):
            raise VMError('vm_not_available_for_automatic_wake')
        self.sample(meta,force=meta['state']=='hibernated'); self.check_credit(meta)
        if meta['state']=='hibernated':
            self.runner.authorize(project,meta['owner_did'])
            with self.admission(project,meta['vcpus'],meta['memory_mib']):
                self.manager.perform(project,meta['owner_did'],'vm/resume',{'vm_id':meta['vm_id']})
            meta=self.manager.read(project,meta['owner_did'])
            meta['manual_stop']=False; meta.pop('runtime_error',None); self.manager.save(meta)
            self.sample(meta)
        self.touch(meta)
        return meta

    @contextmanager
    def http_request(self,project):
        with self.lock(project):
            meta=self.ensure_awake(project)
            state=self.state(meta); state['active_http']+=1
        try: yield meta
        finally:
            with self.lock(project):
                state['active_http']-=1; state['last_incoming']=time.time()

    def disconnect(self,meta):
        for sock in list(self.state(meta)['connections']):
            try: sock.shutdown(2)
            except OSError: pass
            try: sock.close()
            except OSError: pass
        self.state(meta)['connections'].clear()

    def expire(self,meta):
        project,owner=meta['project_id'],meta['owner_did']
        if not self.ledger.claim_expired(project,owner,meta['vm_id']): return
        self.disconnect(meta)
        if self.gateway: self.gateway.withdraw(project)
        # Do not require current approval to enforce deletion after retention.
        # Exact generation and project lifecycle lock still protect new VMs.
        with self.manager.owner_lock(owner):
            if self.manager.alive(meta): self.manager.stop(meta,True)
            if meta['state']!='destroyed':
                self.manager._perform(project,owner,'vm/destroy',{'vm_id':meta['vm_id'],'delete_data':True,'confirm_data_loss':True})
            for path in (self.manager.root/'retained').glob('vm_*'):
                marker=path/'retention.json'
                if marker.exists():
                    who=json.loads(marker.read_text())
                    if who.get('project_id')==project and who.get('owner_did')==owner and path.resolve().parent==self.manager.root/'retained':
                        shutil.rmtree(path)
        self.ledger.finish_deletion(project,owner,meta['vm_id'])

    def tick_project(self,path):
        project=path.stem
        meta=json.loads(path.read_text())
        if meta['state']=='destroyed' and not self.storage_bytes(meta): return
        state=self.state(meta)
        self.sample(meta,force=False)
        account=self.ledger.view(project,meta['owner_did'],include_entries=False)
        if account['deletion_committed'] or (account['billing_mode']=='enforced' and account['delete_after'] is not None and time.time()>=account['delete_after']):
            self.expire(meta); return
        blocked=not account['execution_allowed']
        if not blocked and meta.get('execution_mode')=='always_on' and time.time()-state.get('policy_checked',0)>=30:
            try: approved=self.runner.authorize(project,meta['owner_did']).get('always_on_allowed')
            except Exception: approved=False
            state['policy_checked']=time.time()
            if not approved: meta['execution_mode']='serverless'; self.manager.save(meta)
        blocked=not account['execution_allowed']
        idle=(meta.get('execution_mode','serverless')=='serverless'
              and time.time()-state['last_incoming']>=meta.get('idle_timeout_seconds',900) and not state['active_http'])
        with self.runner.db() as db:
            busy=db.execute("SELECT 1 FROM jobs WHERE project=? AND status IN ('queued','running')",(project,)).fetchone()
        if meta['state']=='running' and (blocked or (idle and not busy)):
            self.disconnect(meta)
            self.manager.hibernate(meta)
            self.sample(meta)
        meta['last_incoming_at']=state['last_incoming']; self.manager.save(meta)
        if meta.get('execution_mode')=='always_on' and meta['state'] in ('hibernated','stopped') and not meta.get('manual_stop') and not blocked and not busy:
            if self.runner.authorize(project,meta['owner_did']).get('always_on_allowed'):
                with self.admission(project,meta['vcpus'],meta['memory_mib']):
                    self.manager.perform(project,meta['owner_did'],'vm/start',{'vm_id':meta['vm_id']})
                self.sample(self.manager.read(project,meta['owner_did']))
                if self.runner.ingress: self.runner.ingress.sync()


    def tick(self):
        errors=[]
        for path in (self.manager.root/'catalog').glob('prj_*.json'):
            with self.lock(path.stem):
                try: self.tick_project(path)
                except Exception as error:
                    errors.append(path.stem+':'+str(error))
                    meta=json.loads(path.read_text())
                    self.disconnect(meta)
                    if self.manager.alive(meta):
                        try: self.manager.hibernate(meta)
                        except Exception: self.manager.stop(meta,True)
                    meta['runtime_error']='runtime_policy_or_meter_unavailable'
                    self.manager.save(meta)
        self.last_error=';'.join(errors) if errors else None

    def run(self):
        while not self.closed:
            try:
                self.tick()
                if self.gateway: self.gateway.reconcile()
            except Exception as error:
                self.last_error=str(error)
            time.sleep(1)
