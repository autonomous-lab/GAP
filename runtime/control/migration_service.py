"""Durable, operator-owned cold migration coordinator.

Client credentials authorize initiation/status only. Node secrets, temporary
project capabilities and private HTTP hop credentials never enter API replies.
"""
import base64
import hashlib
import json
from pathlib import Path
import re
import secrets
import threading
import time
import urllib.request
from urllib.parse import urlsplit

from authority import Failure,identifier
import migration_journal as journal


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self,*args,**kwargs):return None


class Migrations:
    def __init__(self,authority,access,peers,prices):
        self.a=authority;self.access=access;self.prices=prices;self.peers={}
        for node,value in peers.items():
            identifier(node);origin=value['origin'].rstrip('/');parsed=urlsplit(origin)
            token=Path(value['token_file']).read_text().strip()
            if parsed.scheme!='https' or not parsed.hostname or parsed.username or parsed.password or parsed.path or parsed.query or parsed.fragment or not re.fullmatch('[A-Za-z0-9_-]{43,128}',token):raise ValueError('invalid migration peer')
            self.peers[node]=dict(origin=origin,token=token)
        self.guard=threading.Lock();self.tasks=set()
        with self.a.db() as db:
            db.execute('''CREATE TABLE IF NOT EXISTS migration_jobs(
                id TEXT PRIMARY KEY REFERENCES vm_migrations(id),status TEXT NOT NULL,
                error TEXT,received INTEGER NOT NULL DEFAULT 0,total INTEGER NOT NULL DEFAULT 0,
                private TEXT NOT NULL DEFAULT '{}')''')
            pending=[r[0] for r in db.execute("SELECT id FROM migration_jobs WHERE status IN ('queued','running')")]
        for identity in pending:self.launch(identity)

    def call(self,node,path,method='POST',body=None,token=None):
        peer=self.peers[node]
        req=urllib.request.Request(peer['origin']+path,method=method,
            data=None if body is None else json.dumps(body).encode(),headers={
                'Authorization':'Bearer '+(token or peer['token']),
                'Content-Type':'application/json','User-Agent':'GAP-Migration/1.0'})
        try:
            with urllib.request.build_opener(NoRedirect()).open(req,timeout=15) as response:
                raw=response.read(2*1024*1024+1)
                if len(raw)>2*1024*1024:raise ValueError()
                return json.loads(raw)
        except urllib.error.HTTPError as e:
            try:code=json.loads(e.read(8192))['error']['code']
            except Exception:code='migration_node_request_failed'
            if not isinstance(code,str) or not re.fullmatch('[a-z0-9_]{1,100}',code):code='migration_node_request_failed'
            raise Failure(code,409) from None
        except Exception:raise Failure('migration_node_unavailable',503) from None

    def worker(self,node,identity,operation,**body):
        return self.call(node,'/v1/fleet/migration-worker',body=dict(migration_id=identity,operation=operation,**body))

    def cap(self,row,node):
        with self.a.db() as db:customer=journal.row_for(db,row['migration_id'])['customer']
        return self.access.issue(dict(customer=customer,agent=None),row['project_id'],ttl=300,node_id=node)['token']

    def vm_api(self,row,node,suffix,method='GET',body=None):
        return self.call(node,'/v1/cloud/projects/'+row['project_id']+suffix,method,body,self.cap(row,node))

    def permitted(self,actor,project):
        with self.a.db() as db:
            p=db.execute('SELECT * FROM projects WHERE id=? AND customer=?',(project,actor['customer'])).fetchone()
            if not p:raise Failure('project_membership_required',403)
            if actor['agent'] is not None:
                grant=db.execute('SELECT role FROM grants WHERE project=? AND agent=?',(project,actor['agent'])).fetchone()
                if not grant or grant[0] not in ('owner','operator'):raise Failure('project_management_required',403)
            return dict(p)

    def status(self,actor,identity):
        row=journal.get(self.a,identity);self.permitted(actor,row['project_id'])
        with self.a.db() as db:job=db.execute('SELECT status,error,received,total FROM migration_jobs WHERE id=?',(identity,)).fetchone()
        if not job:raise Failure('migration_job_not_found',404)
        return dict(row,job=dict(job))

    def submit(self,actor,body):
        action=body.get('action','start')
        if action!='start':
            identity=body['migration_id'];current=self.status(actor,identity)
            if action=='cancel':
                if current['phase'] not in ('cancelling','target_discarded','cancelled'):
                    journal.cancel_request(self.a,identity+':cancel',identity,current['revision'])
            elif action!='resume':raise Failure('invalid_migration_action')
            if current['job']['status'] not in ('succeeded','cancelled'):
                self.update(identity,status='queued',error=None);self.launch(identity)
            return self.status(actor,identity)
        if body.get('confirm_downtime') is not True:raise Failure('migration_downtime_confirmation_required')
        project=body['project_id'];self.permitted(actor,project)
        request='migration:'+hashlib.sha256((actor['customer']+':'+identifier(body['request_id'])).encode()).hexdigest()
        with self.a.db() as db:previous=db.execute("SELECT result FROM operations WHERE actor='operator' AND id=?",(request,)).fetchone()
        if previous:
            row=json.loads(previous[0]);identity=row['migration_id']
            if (row['project_id'],row['vm_id'],row['target_node'])!=(project,body['vm_id'],body['target_node']):raise Failure('request_id_conflict',409)
            with self.a.db() as db:
                job=db.execute('SELECT private FROM migration_jobs WHERE id=?',(identity,)).fetchone()
                if job and json.loads(job[0])['target_tariff']!=body.get('target_tariff'):raise Failure('request_id_conflict',409)
                db.execute("INSERT OR IGNORE INTO migration_jobs(id,status,private) VALUES(?,'queued',?)",(identity,json.dumps(dict(target_tariff=body['target_tariff']))))
            current=self.status(actor,identity)
            if current['job']['status'] in ('queued','running'):self.launch(identity)
            return current
        with self.a.db() as db:allocation=db.execute("SELECT * FROM capacity WHERE vm=? AND project=? AND state='active'",(body['vm_id'],project)).fetchone()
        if not allocation:raise Failure('migration_requires_active_allocation',409)
        source=allocation['node'];target=body['target_node']
        if target not in self.peers or source not in self.peers:raise Failure('unknown_migration_node')
        prices=self.prices.get()
        if not all(prices.get(n,{}).get('available') for n in (source,target)):raise Failure('migration_pricing_unavailable',409)
        if body.get('target_tariff')!=prices[target]['tariff']:raise Failure('migration_tariff_confirmation_required',409)
        row=journal.prepare(self.a,request,actor['customer'],project,body['vm_id'],source,target,allocation['revision'])
        private=dict(target_tariff=prices[target]['tariff'])
        with self.a.db() as db:db.execute("INSERT OR IGNORE INTO migration_jobs(id,status,private) VALUES(?,'queued',?)",(row['migration_id'],json.dumps(private)))
        self.launch(row['migration_id']);return self.status(actor,row['migration_id'])

    def update(self,identity,**values):
        if set(values)-{'status','error','received','total','private'}:raise ValueError()
        with self.a.db() as db:db.execute('UPDATE migration_jobs SET '+','.join(k+'=?' for k in values)+' WHERE id=?',(*values.values(),identity))

    def private(self,identity):
        with self.a.db() as db:return json.loads(db.execute('SELECT private FROM migration_jobs WHERE id=?',(identity,)).fetchone()[0])

    def launch(self,identity):
        with self.guard:
            if identity in self.tasks:return
            self.tasks.add(identity)
        threading.Thread(target=self.run,args=(identity,),daemon=True).start()

    def wait(self,node,identity,operation,done):
        result=self.worker(node,identity,operation)
        deadline=time.monotonic()+1800
        while result.get('phase') not in done:
            if result.get('phase')=='failed':raise Failure(result.get('error','migration_worker_failed'),409)
            if journal.get(self.a,identity)['phase']=='cancelling' and operation not in ('discard','restore'):return False
            if time.monotonic()>deadline:raise Failure('migration_worker_timeout',409)
            time.sleep(1)
            result=self.worker(node,identity,'status')
            if result.get('operation') not in (None,operation) and result.get('phase') not in ('working','exporting','importing'):
                result=self.worker(node,identity,operation)
        return True

    def prepare(self,row,private):
        identity=row['migration_id'];source=row['source_node'];target=row['target_node']
        a=self.worker(source,identity,'node-admission');b=self.worker(target,identity,'node-admission')
        if not a.get('vm') or a['vm']['state'] not in ('running','stopped','hibernated'):raise Failure('migration_source_not_ready',409)
        if a['kernel_sha256']!=b['kernel_sha256'] or not set(a['cpu_flags'])<=set(b['cpu_flags']):raise Failure('migration_host_incompatible',409)
        if b['memory_available_mib']<a['vm']['memory_mib']+512:raise Failure('migration_target_memory_unavailable',409)
        if min(a['free_bytes'],b['free_bytes'])<2*a['vm']['disk_gib']*1024**3+64*1024**2:raise Failure('migration_temporary_disk_space_unavailable',409)
        if 'hop' not in private:
            existing=False
            with self.a.db() as db:
                known=row['home_node']==target or db.execute('SELECT 1 FROM vm_host_projects WHERE project=? AND node=?',(row['project_id'],target)).fetchone()
            if known:
                settings=self.vm_api(row,target,'/vm/http-access?vm_id='+row['vm_id'])
                existing=settings.get('configured') is True
            hop=self.vm_api(row,target,'/vm/http-access/reveal','POST',dict(vm_id=row['vm_id'])) if existing else dict(username='migration',password=secrets.token_urlsafe(32))
            private.update(hop=dict(username=hop['username'],password=hop['password']),existing_http=existing,was_running=a['vm']['state'] in ('running','hibernated'),ingress=a.get('ingress',{}))
            self.update(identity,private=json.dumps(private))

    def verify_route(self,row,private):
        ingress=private.get('ingress',{})
        if not ingress.get('enabled') or not private['was_running']:return
        url=ingress.get('url','');parsed=urlsplit(url)
        origin=parsed.scheme+'://'+parsed.netloc
        node=next((n for n,p in self.peers.items() if p['origin']==origin),None)
        if node is None or not parsed.path.startswith('/apps/') or parsed.query or parsed.fragment:raise Failure('migration_origin_unavailable',409)
        settings=self.vm_api(row,node,'/vm/http-access?vm_id='+row['vm_id'])
        if not settings.get('configured'):return  # It was not published for visitors.
        credentials=self.vm_api(row,node,'/vm/http-access/reveal','POST',dict(vm_id=row['vm_id']))
        authorization=base64.b64encode((credentials['username']+':'+credentials['password']).encode()).decode()
        request=urllib.request.Request(url,headers={'Authorization':'Basic '+authorization,'User-Agent':'GAP-Migration/1.0'})
        try:
            with urllib.request.build_opener(NoRedirect()).open(request,timeout=30) as response:response.read(4096)
        except urllib.error.HTTPError as error:
            if error.code in (401,502,503,504):raise Failure('migration_https_route_unavailable',409) from None
        except Exception:raise Failure('migration_https_route_unavailable',409) from None

    def transfer(self,row):
        identity=row['migration_id'];source=row['source_node'];target=row['target_node']
        receipt=self.worker(source,identity,'export');uploaded=self.worker(target,identity,'upload-status')
        total=receipt['size'];offset=uploaded['received']
        if offset and (uploaded.get('size'),uploaded.get('archive_sha256'))!=(total,receipt['archive_sha256']):raise Failure('migration_upload_conflict',409)
        self.update(identity,received=offset,total=total)
        while offset<total:
            if journal.get(self.a,identity)['phase']=='cancelling':return
            chunk=self.worker(source,identity,'read',offset=offset)
            if chunk['offset']!=offset or chunk['size']!=total:raise Failure('migration_chunk_conflict',409)
            size=len(base64.b64decode(chunk['data_base64'],validate=True))
            if not 0<size<=512*1024:raise Failure('migration_chunk_conflict',409)
            result=self.worker(target,identity,'write',offset=offset,size=total,archive_sha256=receipt['archive_sha256'],data_base64=chunk['data_base64'])
            if result['received']!=offset+size:raise Failure('migration_chunk_conflict',409)
            offset=result['received'];self.update(identity,received=offset)
        self.wait(target,identity,'import',('imported',))

    def run(self,identity):
        self.update(identity,status='running',error=None)
        try:
            while True:
                row=journal.get(self.a,identity);source=row['source_node'];target=row['target_node'];phase=row['phase'];private=self.private(identity)
                if phase=='prepared':
                    self.prepare(row,private);self.wait(source,identity,'export',('exported',))
                elif phase=='source_fenced':self.transfer(row)
                elif phase=='target_staged':self.wait(source,identity,'settle',('source_settled','routing_ready','committed'))
                elif phase=='source_settled':
                    receipt=self.worker(source,identity,'route',**private['hop'])
                    journal.routes_ready(self.a,identity+':routes',identity,row['revision'],receipt['evidence_id'])
                elif phase=='routing_ready':
                    if self.prices.get().get(target,{}).get('tariff')!=private['target_tariff']:raise Failure('migration_tariff_changed_cancel_and_restart',409)
                    journal.commit(self.a,identity+':commit',identity,row['revision'])
                elif phase=='committed':
                    self.vm_api(row,target,'/provision','POST',{})
                    self.wait(target,identity,'activate',('activated',))
                    if not private['existing_http']:self.vm_api(row,target,'/vm/http-access','PUT',dict(vm_id=row['vm_id'],**private['hop']))
                    target_state=self.worker(target,identity,'binding')
                    if private['was_running'] and target_state['vm']['state']!='running':raise Failure('migration_target_not_running',409)
                    self.verify_route(row,private)
                    self.wait(source,identity,'cleanup',('cleaned',))
                    self.worker(target,identity,'prune')
                    self.update(identity,status='succeeded',error=None);return
                elif phase=='cancelling':self.wait(target,identity,'discard',('discarded',))
                elif phase=='target_discarded':self.wait(source,identity,'restore',('restored',))
                elif phase=='cancelled':self.update(identity,status='cancelled',error=None);return
                else:raise Failure('migration_phase_conflict',409)
        except Exception as error:
            code=error.code if isinstance(error,Failure) else 'migration_coordinator_failed'
            self.update(identity,status='failed',error=code)
        finally:
            with self.guard:self.tasks.discard(identity)
