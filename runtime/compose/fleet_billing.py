"""Durable reservation adapter. A lease is process-local and never revived on boot.

Only explicitly configured, financially empty projects may opt in. Persisted
bindings fence the legacy spending path even if configuration is later removed.
"""
import json
from pathlib import Path
import re
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

from billing import Ledger, BillingError, UNITS
import finance


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self,*args,**kwargs):
        return None


class Client:
    def __init__(self,config):
        self.config=config
        parsed=urllib.parse.urlsplit(config['url'])
        if parsed.username or parsed.password or parsed.query or parsed.fragment or parsed.path not in ('','/','/v1/fleet'):
            raise ValueError('invalid_fleet_authority_url')
        if parsed.scheme!='https' and not (parsed.scheme=='http' and parsed.hostname in ('127.0.0.1','localhost','172.17.0.1')):
            raise ValueError('fleet_authority_requires_tls_or_local_tunnel')
        self.url=config['url'].rstrip('/')+'/node'
        self.token=Path(config['token_file']).read_text().strip()
        if not re.fullmatch(r'[A-Za-z0-9_-]{43,128}',self.token):
            raise ValueError('invalid_fleet_node_credential')

    def __call__(self,body):
        request=urllib.request.Request(self.url,data=json.dumps(body).encode(),
            headers={'Authorization':'Bearer '+self.token,'Content-Type':'application/json','User-Agent':'GAP-Worker/1.0'})
        try:
            with urllib.request.build_opener(NoRedirect()).open(request,timeout=2) as response:
                raw=response.read(65537)
            if len(raw)>65536:raise ValueError()
            result=json.loads(raw)
            if not isinstance(result,dict):raise ValueError()
            return result
        except urllib.error.HTTPError as error:
            code='fleet_authority_denied' if error.code in (401,403) else 'fleet_reconciliation_required' if error.code==409 else 'fleet_authority_unavailable'
            if str(body.get('action','')).startswith('capacity-'):
                try:
                    value=json.loads(error.read(65537)).get('error',{}).get('code')
                    if value in ('capacity_not_found','capacity_released','capacity_revision_conflict',
                                 'capacity_transition_pending','capacity_transition_mismatch','capacity_disabled',
                                 'customer_quota_exceeded_max_vms','customer_quota_exceeded_cpu_quarters',
                                 'customer_quota_exceeded_memory_mib'):
                        code=value
                except (ValueError,TypeError,AttributeError):pass
            error.close()
            raise BillingError(code) from None
        except (OSError,ValueError):
            # No error message/URL/request echo from an upstream endpoint.
            raise BillingError('fleet_authority_unavailable') from None


class FleetLedger(Ledger):
    def __init__(self,path,config,clock=time.time,monotonic=time.monotonic,transport=None):
        self.config=config
        self.projects=set(config['projects'])
        if any(not isinstance(p,str) or not re.fullmatch(r'prj_[0-9a-f]{24}',p) for p in self.projects):
            raise ValueError('invalid_fleet_project')
        for key in ('operator_id','node_id'):
            if not re.fullmatch(r'[A-Za-z0-9_.:-]{1,100}',config[key]):raise ValueError('invalid_fleet_identity')
        self.target=config.get('target_microcredits',100000)
        self.lease_seconds=config.get('lease_seconds',30)
        if type(self.target) is not int or not 1<=self.target<=1000000 or type(self.lease_seconds) is not int or not 10<=self.lease_seconds<=60:
            raise ValueError('invalid_fleet_limits')
        self.monotonic=monotonic
        self.transport=transport or Client(config)
        self.deadlines={};self.last_sync={};self.errors={};self.funding={}
        self.locks={p:threading.RLock() for p in self.projects}
        super().__init__(path,clock)

    def fleet_allows(self,project):
        return project in self.projects

    def lease_allowed(self,project):
        if project in self.projects:
            return self.monotonic()<self.deadlines.get(project,0)
        return super().lease_allowed(project)

    def ensure(self,db,project,owner):
        account=super().ensure(db,project,owner)
        binding=db.execute('SELECT * FROM fleet_bindings WHERE project=?',(project,)).fetchone()
        if binding and project in self.projects and (binding['operator'],binding['node'])!=(self.config['operator_id'],self.config['node_id']):
            raise BillingError('fleet_authority_binding_mismatch')
        if project in self.projects and not binding:
            mode,_=self.tariff(db)
            if mode!='enforced':raise BillingError('fleet_requires_enforced_billing')
            if any(account[k] for k in ('balance','spent','estimated','remainder','shadow_remainder','retention_claim')) or db.execute('SELECT 1 FROM entries WHERE project=? LIMIT 1',(project,)).fetchone():
                raise BillingError('legacy_wallet_migration_required')
            db.execute('INSERT INTO fleet_bindings(project,owner,reservation,operator,node) VALUES(?,?,?,?,?)',
                       (project,owner,'rsv_'+uuid.uuid4().hex,self.config['operator_id'],self.config['node_id']))
            self.fenced_projects.add(project)
        return account

    def _charge(self,db,project,owner,*args,**kwargs):
        result=super()._charge(db,project,owner,*args,**kwargs)
        if project in self.projects and db.execute('SELECT balance FROM accounts WHERE project=?',(project,)).fetchone()[0]==0:
            self.deadlines.pop(project,None)
        return result

    def sync(self,project,owner,force=False):
        if project not in self.projects:return
        with self.locks[project]:
            started=self.monotonic()
            if not force and self.lease_allowed(project) and started-self.last_sync.get(project,0)<3:return
            with self.db() as db:
                account=self.ensure(db,project,owner)
                row=db.execute('SELECT * FROM fleet_bindings WHERE project=?',(project,)).fetchone()
                pending=json.loads(row['pending']) if row['pending'] else dict(action='checkpoint',request_id=uuid.uuid4().hex,
                    project_id=project,owner_did=owner,reservation_id=row['reservation'],
                    consumed_microcredits=account['spent']-row['legacy_spent'],
                    unpaid_microcredits=max(0,(account['estimated']-row['legacy_estimated'])-(account['spent']-row['legacy_spent'])),
                    target_microcredits=self.target,lease_seconds=self.lease_seconds)
                encoded=json.dumps(pending,sort_keys=True)
                db.execute('UPDATE fleet_bindings SET pending=? WHERE project=?',(encoded,project))
            try:
                reply=self.transport(pending)
                expected=dict(operator_id=self.config['operator_id'],node_id=self.config['node_id'],project_id=project,
                              owner_did=owner,reservation_id=pending['reservation_id'],closed=False)
                if any(reply.get(k)!=v for k,v in expected.items()):raise ValueError()
                for key in ('allocated_microcredits','consumed_microcredits','lease_expires_at','authority_now'):
                    if type(reply.get(key)) is not int or reply[key]<0:raise ValueError()
                if reply['consumed_microcredits']!=pending['consumed_microcredits']:raise ValueError()
                if reply.get('funding_status') not in ('available','fully_reserved','exhausted'):raise ValueError()
                remaining=max(0,min(self.lease_seconds,reply['lease_expires_at']-reply['authority_now']))
                with self.db() as db:
                    account=self.ensure(db,project,owner)
                    row=db.execute('SELECT * FROM fleet_bindings WHERE project=?',(project,)).fetchone()
                    if row['pending']!=encoded:return  # Another process already acknowledged it.
                    extra=reply['allocated_microcredits']-row['allocated']
                    if extra<0 or extra>pending['target_microcredits']:raise ValueError()
                    balance=account['balance']+extra
                    # Retained storage remains metered during an outage. Repay
                    # accumulated unpaid usage before permitting execution again.
                    arrears=min(balance,max(0,account['estimated']-account['spent']))
                    balance-=arrears
                    db.execute('UPDATE accounts SET balance=?,spent=spent+?,budget_spent=budget_spent+?,exhausted_at=NULL WHERE project=?',
                               (balance,arrears,arrears,project))
                    if arrears:
                        result=dict(kind='arrears_payment',estimated_microcredits=0,debited_microcredits=arrears,
                                    unpaid_microcredits=-arrears,usage={k:0 for k in UNITS})
                        key='fleet-arrears:'+pending['request_id']
                        hashed,_=self.operation(db,project,key,{'amount':arrears})
                        self.entry(db,project,key,hashed,result)
                        finance.record(db,project,result,self.clock(),self.clock())
                    db.execute('UPDATE fleet_bindings SET allocated=?,pending=NULL WHERE project=?',
                               (reply['allocated_microcredits'],project))
                self.deadlines[project]=started+remaining if balance else 0
                self.last_sync[project]=started
                self.funding[project]=reply['funding_status']
                self.errors.pop(project,None)
            except BillingError as error:
                self.errors[project]=str(error) if str(error) in ('fleet_authority_unavailable','fleet_authority_denied','fleet_reconciliation_required') else 'fleet_authority_unavailable'
                # Keep only the original, unextended, in-process deadline.
            except (ValueError,KeyError,TypeError):
                self.errors[project]='fleet_authority_invalid_response'

    def sample(self,meta,*args,**kwargs):
        super().sample(meta,*args,**kwargs)
        self.sync(meta['project_id'],meta['owner_did'],force=True)

    def view(self,project,owner,include_entries=True):
        result=super().view(project,owner,include_entries)
        if project in self.projects:
            valid=self.lease_allowed(project)
            result.update(balance_scope='node_reservation',exhausted_at=None,delete_after=None,
                          deletion_committed=False,execution_allowed=result['execution_allowed'] and valid,
                          fleet={'operator_id':self.config['operator_id'],'node_id':self.config['node_id'],
                                 'lease_valid':valid,'authority_error':self.errors.get(project),
                                 'funding_status':self.funding.get(project,'unknown'),
                                 'retention':'preserve_until_authoritative_reconciliation'})
            result['alerts']=[a for a in result['alerts'] if a!='credits_exhausted']
            if not valid:result['alerts'].append('fleet_allowance_or_lease_unavailable')
        return result
