"""Durable VM capacity intents, independent of disposable job payloads.

The caller serializes mutations under the owner/project lifecycle locks. Recovery
never replays disk creation or resize: it reconciles the durable catalog, or
fences an absent generation before cancelling its possibly delayed reservation.
"""
from contextlib import contextmanager
import json
import os
import re
import sqlite3
import uuid

from billing import BillingError
from cpu_quota import quarters
from fleet_billing import Client
from microvm import VMError


class Capacity:
    def __init__(self, manager):
        self.manager=manager
        self.path=manager.root/'fleet-capacity.sqlite'
        fd=os.open(self.path,os.O_CREAT|os.O_RDWR|os.O_NOFOLLOW,0o600);os.close(fd)
        self.config=None;self.projects=set();self.ready=set();self.transport=None
        with self.db() as db:
            db.execute('CREATE TABLE IF NOT EXISTS bindings(project TEXT PRIMARY KEY,owner TEXT NOT NULL,operator TEXT NOT NULL,node TEXT NOT NULL)')
            db.execute('CREATE TABLE IF NOT EXISTS intents(vm TEXT PRIMARY KEY,project TEXT NOT NULL,owner TEXT NOT NULL,data TEXT NOT NULL)')
            self.bound={r['project'] for r in db.execute('SELECT project FROM bindings')}

    @contextmanager
    def db(self):
        db=sqlite3.connect(self.path,timeout=2,isolation_level=None);db.row_factory=sqlite3.Row
        try:
            db.execute('PRAGMA journal_mode=WAL');db.execute('PRAGMA synchronous=FULL');db.execute('BEGIN IMMEDIATE')
            yield db
            db.execute('COMMIT')
        except BaseException:
            if db.in_transaction:db.execute('ROLLBACK')
            raise
        finally:db.close()

    def configure(self, config, transport=None):
        self.config=config;self.projects=set(config['projects']) if config else set();self.ready.clear()
        self.transport=(transport or Client(config)) if config else None
        with self.db() as db:
            for row in db.execute('SELECT * FROM bindings'):
                if row['project'] in self.projects and (row['operator'],row['node'])!=(config['operator_id'],config['node_id']):
                    raise VMError('fleet_capacity_binding_mismatch')

    def managed(self, project):
        return project in self.projects or project in self.bound

    def require(self, project):
        if project not in self.projects:raise VMError('fleet_capacity_configuration_required')

    def bind(self, project, owner):
        self.require(project)
        with self.db() as db:
            old=db.execute('SELECT * FROM bindings WHERE project=?',(project,)).fetchone()
            if old:
                if old['owner']!=owner:raise VMError('fleet_capacity_owner_mismatch')
                return
            if any(m['state']!='destroyed' for m in self.manager.list(project,owner)):
                raise VMError('legacy_vm_capacity_migration_required')
            db.execute('INSERT INTO bindings VALUES(?,?,?,?)',(project,owner,self.config['operator_id'],self.config['node_id']))
        self.bound.add(project)

    def fence_legacy_wallet(self, project, owner, config):
        if not re.fullmatch(r'prj_[0-9a-f]{24}',project) or not re.fullmatch(r'did:gap:[0-9a-f]{64}',owner):
            raise VMError('invalid_migration_identity')
        with self.db() as db:
            old=db.execute('SELECT * FROM bindings WHERE project=?',(project,)).fetchone()
            expected=(owner,config['operator_id'],config['node_id'])
            if old and (old['owner'],old['operator'],old['node'])!=expected:
                raise VMError('fleet_capacity_binding_mismatch')
            if any(m['state']!='destroyed' for m in self.manager.list(project,owner)):
                raise VMError('legacy_vm_capacity_migration_required')
            db.execute('INSERT OR IGNORE INTO bindings VALUES(?,?,?,?)',(project,*expected))
        self.bound.add(project)

    def put(self, record):
        with self.db() as db:
            db.execute('INSERT OR REPLACE INTO intents VALUES(?,?,?,?)',
                       (record['vm'],record['project'],record['owner'],json.dumps(record,sort_keys=True)))

    def records(self, project=None):
        with self.db() as db:
            rows=db.execute('SELECT data FROM intents'+(' WHERE project=?' if project else ''),(project,) if project else ())
            return [json.loads(r[0]) for r in rows]

    def meta(self, record):
        # A missing old default generation is not the new default VM.
        return next((m for m in self.manager.list(record['project'],record['owner']) if m['vm_id']==record['vm']),None)

    @staticmethod
    def resources(meta):
        return dict(cpu_quarters=quarters(meta['vcpus']),memory_mib=meta['memory_mib'])

    def call(self, record, body):
        self.require(record['project'])
        try:reply=self.transport(body)
        except BillingError as e:raise VMError(str(e)) from None
        if (reply.get('operator_id'),reply.get('node_id'),reply.get('project_id'),reply.get('vm_id')) != (
            self.config['operator_id'],self.config['node_id'],record['project'],record['vm']):
            raise VMError('fleet_capacity_invalid_response')
        if type(reply.get('revision')) is not int or reply['revision']<1 or reply.get('state') not in ('pending','active','released'):
            raise VMError('fleet_capacity_invalid_response')
        for key in ('committed','target'):
            value=reply.get(key)
            if not isinstance(value,dict) or set(value)!={'cpu_quarters','memory_mib'} or any(type(n) is not int or n<0 for n in value.values()):
                raise VMError('fleet_capacity_invalid_response')
        if reply['state']=='pending' and not isinstance(reply.get('transition_id'),str):
            raise VMError('fleet_capacity_invalid_response')
        return reply

    def current(self, record):
        return self.call(record,dict(action='capacity-get',project_id=record['project'],vm_id=record['vm']))

    def finish(self, record, remote, outcome):
        record['finish']=dict(action='capacity-finish',request_id=uuid.uuid4().hex,
            project_id=record['project'],vm_id=record['vm'],expected_revision=remote['revision'],
            transition_id=remote['transition_id'],outcome=outcome,evidence_id='local-'+uuid.uuid4().hex)
        record['phase']='finishing';self.put(record)
        self.call(record,record['finish'])

    def reconcile(self, record):
        self.require(record['project']);self.ready.discard(record['vm'])
        if record['phase']=='closed':return
        meta=self.meta(record)
        if record['phase']=='fenced' or (not meta and record['kind']=='create'):
            if meta and meta['state']!='destroyed':raise VMError('fleet_capacity_fenced_generation_present')
            # Persist the local tombstone BEFORE the remote cancellation, which
            # may win a race against a prepare still queued on the authority.
            record['phase']='fenced'
            record.setdefault('cancel',dict(action='capacity-cancel-create',request_id=uuid.uuid4().hex,
                project_id=record['project'],owner_did=record['owner'],vm_id=record['vm'],evidence_id='local-'+uuid.uuid4().hex))
            self.put(record);self.call(record,record['cancel'])
            remote=self.current(record)
            if remote['state']!='released':raise VMError('fleet_capacity_reconciliation_required')
            record['phase']='closed';record['remote']=remote;self.put(record);return
        if not meta:raise VMError('fleet_capacity_catalog_missing')
        if record['phase']=='destroying' and meta['state']!='destroyed':
            # Resume only the exact, already authorized local destruction.
            # Its retained/delete choice is durable before touching any disk.
            if self.manager.alive(meta):raise VMError('fleet_capacity_pending_process_alive')
            self.manager._perform(record['project'],record['owner'],'vm/destroy',record['destroy'])
            meta=self.meta(record)
        if (record['kind']=='resize' and record['phase'] in ('preparing','applying','rolling_back')
                and self.resources(meta)==record['previous']):
            if self.manager.alive(meta):raise VMError('fleet_capacity_pending_process_alive')
            record['phase']='rolling_back'
            record.setdefault('abort',dict(action='capacity-abort-resize',request_id=uuid.uuid4().hex,
                project_id=record['project'],vm_id=record['vm'],expected_revision=record['prepare']['expected_revision'],
                transition_id=record['prepare']['request_id'],evidence_id='local-'+uuid.uuid4().hex))
            self.put(record);self.call(record,record['abort'])
        if record.get('finish'):
            self.call(record,record['finish'])  # exact durable replay, including lost release acknowledgements
        remote=self.current(record)
        if meta['state']=='destroyed':
            if self.manager.alive(meta):raise VMError('fleet_capacity_destroyed_process_alive')
            if remote['state']=='pending':
                # A partially created VM can be destroyed after its creation
                # was reconciled; pending resize must first settle from catalog.
                raise VMError('fleet_capacity_pending_destruction')
            if remote['state']=='active':
                self.finish(record,remote,'release');remote=self.current(record)
            if remote['state']!='released':raise VMError('fleet_capacity_reconciliation_required')
            record.update(phase='closed',remote=remote);self.put(record);return
        resources=self.resources(meta)
        if remote['state']=='pending':
            if self.manager.alive(meta):raise VMError('fleet_capacity_pending_process_alive')
            if resources==remote['target']:outcome='commit'
            elif record['kind']=='resize' and resources==remote['committed']:outcome='abort'
            else:raise VMError('fleet_capacity_resource_mismatch')
            self.finish(record,remote,outcome);remote=self.current(record)
        if remote['state']!='active' or remote['committed']!=resources:
            raise VMError('fleet_capacity_resource_mismatch')
        record.update(phase='active',remote=remote);record.pop('finish',None);self.put(record)
        self.ready.add(record['vm'])

    def reconcile_project(self, project, owner):
        self.require(project)
        for record in self.records(project):
            if record['owner']!=owner:raise VMError('fleet_capacity_owner_mismatch')
            if record['phase']!='closed' and (record['phase']!='active' or record['vm'] not in self.ready):
                self.reconcile(record)

    def allows(self, meta):
        if not self.managed(meta['project_id']):return True
        if meta['project_id'] not in self.projects or meta['vm_id'] not in self.ready:return False
        record=next((r for r in self.records(meta['project_id']) if r['vm']==meta['vm_id']),None)
        return bool(record and record['phase']=='active' and record['remote']['committed']==self.resources(meta))

    def execute(self, project, owner, action, body):
        """Called under MicroVMs.perform's owner lock and caller's lifecycle lock."""
        self.bind(project,owner);self.reconcile_project(project,owner)
        manager=self.manager
        meta=None if action=='vm/create' and body.get('new_vm') else manager.read(project,owner,body.get('vm_id'))
        if action=='vm/create':
            if meta and meta['state']!='destroyed':raise VMError('vm_already_exists')
            vm='vm_'+uuid.uuid4().hex
            record=dict(vm=vm,project=project,owner=owner,kind='create',phase='preparing')
            target=dict(vcpus=body.get('vcpus',1),memory_mib=body.get('memory_mib',1024))
            revision=0
        else:
            if not meta or meta['state']=='destroyed':raise VMError('vm_not_found')
            record=next((r for r in self.records(project) if r['vm']==meta['vm_id']),None)
            if not record or record['phase']!='active':raise VMError('fleet_capacity_reservation_required')
            if action not in ('vm/update','vm/destroy'):
                return manager._perform(project,owner,action,body)
            if manager.alive(meta):raise VMError('stop_vm_before_reconfiguration_or_destruction')
            if action=='vm/destroy':
                record['phase']='destroying';record['destroy']=dict(body)
                self.ready.discard(record['vm']);self.put(record)
                try:result=manager._perform(project,owner,action,body)
                finally:self.reconcile(record)
                return result
            if meta['state']=='hibernated':raise VMError('resume_then_stop_before_resize')
            record.pop('abort',None)
            record.update(kind='resize',phase='preparing',previous=self.resources(meta));target=dict(meta,**{k:body[k] for k in ('vcpus','memory_mib') if k in body})
            revision=record['remote']['revision']
        record['prepare']=dict(action='capacity-prepare',request_id=uuid.uuid4().hex,
            project_id=project,owner_did=owner,vm_id=record['vm'],expected_revision=revision,**self.resources(target))
        self.ready.discard(record['vm']);self.put(record)
        try:
            self.call(record,record['prepare'])
            remote=self.current(record)
            if remote['state']!='pending' or remote['transition_id']!=record['prepare']['request_id']:
                raise VMError('fleet_capacity_stale_prepare')
            record['phase']='applying';self.put(record)
            # Never execute guest instructions until the local allocation has
            # been acknowledged centrally and the credit/policy gates pass.
            result=manager._perform(project,owner,action,body,created_vm_id=record['vm'] if action=='vm/create' else None,defer_start=True)
            self.reconcile(record)
            if action=='vm/create' and body.get('start',True):
                with manager.lock(project):
                    meta=self.meta(record);manager.start(meta);result={'ok':True,'vm':manager.public(meta)}
            return result
        except Exception:
            try:self.reconcile(record)
            except Exception:pass  # keep the durable intent and fail closed for recovery
            raise
