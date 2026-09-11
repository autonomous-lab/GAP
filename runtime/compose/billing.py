"""Transactional prepaid microVM credit ledger; integer prices, no minute rounding.

One credit = 1,000,000 microcredits. Metering carries fractional microcredits
between samples. Realtime's existing project credit accounts remain separate.
"""
from contextlib import contextmanager
from fractions import Fraction
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import time
import uuid

GIB = 1024**3
DENOMINATOR = GIB * 3_600_000
RETENTION_SECONDS = 3 * 24 * 3600
RATES = ('vcpu_hour', 'gib_ram_hour', 'gib_disk_hour', 'gib_in', 'gib_out')
COMMERCIAL_RATES = ('vcpu_hour', 'gib_ram_hour', 'gb_disk_month', 'gb_in', 'gb_out')
MONTH_HOURS = 730
UNITS = ('vcpu_ms', 'ram_byte_ms', 'disk_byte_ms', 'bytes_in', 'bytes_out')


class BillingError(ValueError):
    pass


def integer(value, minimum=0, maximum=10**15):
    if type(value) is not int or not minimum <= value <= maximum:
        raise BillingError('invalid_billing_integer')
    return value


class Ledger:
    def __init__(self, path, clock=time.time):
        self.path, self.clock = str(path), clock
        with self.db() as db:
            db.executescript('''
            CREATE TABLE IF NOT EXISTS settings(id INTEGER PRIMARY KEY CHECK(id=1), mode TEXT NOT NULL, tariff TEXT);
            INSERT OR IGNORE INTO settings VALUES(1,'shadow',NULL);
            CREATE TABLE IF NOT EXISTS tariffs(version TEXT PRIMARY KEY, payload TEXT NOT NULL, created REAL NOT NULL);
            CREATE TABLE IF NOT EXISTS accounts(project TEXT PRIMARY KEY, owner TEXT NOT NULL,
                balance INTEGER NOT NULL DEFAULT 0, spent INTEGER NOT NULL DEFAULT 0,
                estimated INTEGER NOT NULL DEFAULT 0, remainder INTEGER NOT NULL DEFAULT 0, shadow_remainder INTEGER NOT NULL DEFAULT 0,
                exhausted_at REAL, budget INTEGER, budget_spent INTEGER NOT NULL DEFAULT 0,
                budget_epoch INTEGER NOT NULL DEFAULT 0, retention_claim TEXT);
            CREATE TABLE IF NOT EXISTS entries(id INTEGER PRIMARY KEY AUTOINCREMENT,
                project TEXT NOT NULL, operation TEXT NOT NULL, digest TEXT NOT NULL,
                payload TEXT NOT NULL, created REAL NOT NULL, UNIQUE(project,operation));
            CREATE TABLE IF NOT EXISTS samples(vm TEXT PRIMARY KEY, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS legacy_meter_entries(id INTEGER PRIMARY KEY,
                project TEXT NOT NULL, operation TEXT NOT NULL, digest TEXT NOT NULL,
                payload TEXT NOT NULL, created REAL NOT NULL);
            ''')
            if not db.in_transaction: db.execute('BEGIN IMMEDIATE')
            self.compact_legacy(db)
        Path(self.path).chmod(0o600)

    def compact_legacy(self, db):
        """Keep old raw measurements for audit; display consolidated historical periods.

        Old measurements did not record lifecycle state. Infer ON/OFF only,
        explicitly labelled historical; never invent stopped vs hibernated.
        This migration changes no account balance, carry, budget or checkpoint.
        """
        group = None
        for row in list(db.execute("SELECT * FROM entries WHERE operation LIKE 'meter:%' ORDER BY project,id")):
            payload = json.loads(row['payload'])
            parts = row['operation'].split(':')
            if payload.get('kind') != 'usage' or len(parts) != 3: continue
            vm = parts[1]
            state = 'historical_on' if payload['usage']['vcpu_ms'] else 'historical_off'
            profile = (row['project'], vm, state, payload['billing_mode'], payload['tariff_version'])
            db.execute('INSERT OR IGNORE INTO legacy_meter_entries VALUES(?,?,?,?,?,?)', tuple(row))
            if group is None or group[0] != profile:
                key = 'history-period:' + str(row['id'])
                group = (profile, key)
            period = {'vm_id': vm, 'state': state, 'started_at': row['created'],
                      'ended_at': row['created'], 'historical': True}
            self.accumulate(db, row['project'], group[1], payload, period)
            db.execute('DELETE FROM entries WHERE id=?', (row['id'],))

    def accumulate(self, db, project, key, result, period):
        row = db.execute('SELECT payload FROM entries WHERE project=? AND operation=?', (project,key)).fetchone()
        if row:
            combined = json.loads(row[0])
            for unit in UNITS: combined['usage'][unit] += result['usage'][unit]
            for field in ('estimated_microcredits','debited_microcredits','unpaid_microcredits'):
                combined[field] += result[field]
            combined['ended_at'] = period['ended_at']
            combined['sample_count'] += 1
        else:
            combined = dict(result, **period, sample_count=1)
        encoded = json.dumps(combined, sort_keys=True)
        digest = hashlib.sha256(encoded.encode()).hexdigest()
        if row:
            db.execute('UPDATE entries SET payload=?,digest=? WHERE project=? AND operation=?',
                       (encoded,digest,project,key))
        else:
            db.execute('INSERT INTO entries(project,operation,digest,payload,created) VALUES(?,?,?,?,?)',
                       (project,key,digest,encoded,period['started_at']))

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.path, timeout=30, isolation_level=None)
        db.row_factory = sqlite3.Row
        db.execute('PRAGMA journal_mode=WAL')
        db.execute('PRAGMA synchronous=FULL')
        db.execute('BEGIN IMMEDIATE')
        try:
            yield db
            if db.in_transaction: db.execute('COMMIT')
        except BaseException:
            if db.in_transaction: db.execute('ROLLBACK')
            raise
        finally:
            db.close()

    def ensure(self, db, project, owner):
        if not re.fullmatch(r'prj_[0-9a-f]{24}', project) or not re.fullmatch(r'did:gap:[0-9a-f]{64}', owner):
            raise BillingError('invalid_billing_identity')
        db.execute('INSERT OR IGNORE INTO accounts(project,owner) VALUES(?,?)', (project,owner))
        account = db.execute('SELECT * FROM accounts WHERE project=?',(project,)).fetchone()
        if account['owner'] != owner: raise BillingError('billing_owner_mismatch')
        return account

    def tariff(self, db):
        settings = dict(db.execute('SELECT * FROM settings WHERE id=1').fetchone())
        row = db.execute('SELECT payload FROM tariffs WHERE version=?',(settings['tariff'],)).fetchone()
        return settings['mode'], json.loads(row[0]) if row else None

    def pricing(self):
        with self.db() as db:
            mode, tariff = self.tariff(db)
            return {'mode':mode,'tariff':tariff,'microcredits_per_credit':1_000_000,
                    'retention_seconds':RETENTION_SECONDS}

    def set_pricing(self, mode, tariff=None):
        if mode not in ('shadow','enforced'): raise BillingError('invalid_billing_mode')
        with self.db() as db:
            _, current = self.tariff(db)
            if tariff is not None:
                if not isinstance(tariff,dict) or set(tariff) not in ({'version',*RATES}, {'version',*COMMERCIAL_RATES}):
                    raise BillingError('invalid_tariff')
                if not isinstance(tariff['version'],str) or not re.fullmatch(r'[A-Za-z0-9_.-]{1,64}',tariff['version']):
                    raise BillingError('invalid_tariff_version')
                for key in set(tariff)-{'version'}: integer(tariff[key],0,10**12)
                encoded=json.dumps(tariff,sort_keys=True)
                existing=db.execute('SELECT payload FROM tariffs WHERE version=?',(tariff['version'],)).fetchone()
                if existing and existing[0]!=encoded: raise BillingError('immutable_tariff_version')
                db.execute('INSERT OR IGNORE INTO tariffs VALUES(?,?,?)',(tariff['version'],encoded,self.clock()))
                current=tariff
            if mode=='enforced' and (not current or not any(current[k] for k in set(current)-{'version'})):
                raise BillingError('nonzero_tariff_required_for_enforcement')
            db.execute('UPDATE settings SET mode=?,tariff=? WHERE id=1',(mode,current['version'] if current else None))
            for sample_row in list(db.execute('SELECT vm,payload FROM samples')):
                sample=json.loads(sample_row['payload'])
                if sample['mode']!=mode or sample['tariff']!=current: sample.pop('period_id',None)
                sample.update(mode=mode,tariff=current)
                db.execute('UPDATE samples SET payload=? WHERE vm=?',(json.dumps(sample),sample_row['vm']))
            if mode=='enforced':
                db.execute('UPDATE accounts SET exhausted_at=COALESCE(exhausted_at,?) WHERE balance=0',(self.clock(),))
        return self.pricing()

    def operation(self, db, project, key, body):
        if not isinstance(key,str) or not re.fullmatch(r'[A-Za-z0-9_.:-]{1,128}',key):
            raise BillingError('invalid_billing_operation')
        digest=hashlib.sha256(json.dumps(body,sort_keys=True).encode()).hexdigest()
        existing=db.execute('SELECT digest,payload FROM entries WHERE project=? AND operation=?',(project,key)).fetchone()
        if existing and existing['digest']!=digest: raise BillingError('billing_operation_conflict')
        return digest, json.loads(existing['payload']) if existing else None

    def entry(self,db,project,key,digest,payload):
        db.execute('INSERT INTO entries(project,operation,digest,payload,created) VALUES(?,?,?,?,?)',
                   (project,key,digest,json.dumps(payload),self.clock()))

    def topup(self,project,owner,amount,key):
        integer(amount,1)
        with self.db() as db:
            account=self.ensure(db,project,owner)
            digest,old=self.operation(db,project,'topup:'+key,{'amount':amount})
            if old is not None: return old
            if account['retention_claim']: raise BillingError('storage_deletion_already_committed')
            balance=integer(account['balance']+amount)
            db.execute('UPDATE accounts SET balance=?,exhausted_at=NULL WHERE project=?',(balance,project))
            result={'kind':'topup','amount_microcredits':amount,'balance_microcredits':balance}
            self.entry(db,project,'topup:'+key,digest,result)
            return result

    def set_budget(self,project,owner,amount,key):
        if amount is not None: integer(amount,1)
        with self.db() as db:
            account=self.ensure(db,project,owner)
            digest,old=self.operation(db,project,'budget:'+key,{'amount':amount})
            if old is not None: return old
            db.execute('UPDATE accounts SET budget=?,budget_spent=0,budget_epoch=budget_epoch+1 WHERE project=?',(amount,project))
            result={'kind':'budget','budget_microcredits':amount,'budget_epoch':account['budget_epoch']+1}
            self.entry(db,project,'budget:'+key,digest,result)
            return result

    def _charge(self,db,project,owner,key,usage,mode,tariff,period=None):
        if set(usage)!=set(UNITS) or any(type(v) is not int or v<0 for v in usage.values()):
            raise BillingError('invalid_metering_usage')
        account=self.ensure(db,project,owner)
        body={'usage':usage,'mode':mode,'tariff':tariff}
        if period is None:
            digest,old=self.operation(db,project,key,body)
            if old is not None: return old
        remainder_column='remainder' if mode=='enforced' else 'shadow_remainder'
        # SQLite preserves fractional carry as rational text; legacy integer
        # remainders retain exactly the same denominator and value.
        numerator=Fraction(account[remainder_column])
        if tariff:
            if 'gb_disk_month' in tariff:
                disk_rate=Fraction(tariff['gb_disk_month']*GIB,10**9*MONTH_HOURS)
                in_rate=Fraction(tariff['gb_in']*GIB,10**9)
                out_rate=Fraction(tariff['gb_out']*GIB,10**9)
            else:
                disk_rate,in_rate,out_rate=(tariff[k] for k in RATES[2:])
            numerator+=(usage['vcpu_ms']*GIB*tariff['vcpu_hour']
                +usage['ram_byte_ms']*tariff['gib_ram_hour']
                +usage['disk_byte_ms']*disk_rate
                +(usage['bytes_in']*in_rate+usage['bytes_out']*out_rate)*3_600_000)
        cost,remainder=divmod(numerator,DENOMINATOR)
        # Budget is an execution stop threshold. Persistent storage remains
        # billable after execution stops; never turn a budget into free disk.
        debit=min(account['balance'],cost) if mode=='enforced' else 0
        exhausted=account['exhausted_at']
        if mode=='enforced' and account['balance']-debit==0 and exhausted is None:
            exhausted=self.clock()
        db.execute(f'UPDATE accounts SET balance=balance-?,spent=spent+?,estimated=estimated+?,{remainder_column}=?,exhausted_at=?,budget_spent=budget_spent+? WHERE project=?',
                   (debit,debit,cost,str(remainder),exhausted,debit,project))
        result={'kind':'usage','usage':usage,'tariff_version':tariff['version'] if tariff else None,
                'billing_mode':mode,'estimated_microcredits':cost,'debited_microcredits':debit,
                'unpaid_microcredits':cost-debit if mode=='enforced' else 0}
        if period is None: self.entry(db,project,key,digest,result)
        else: self.accumulate(db,project,key,result,period)
        return result

    def sample(self,meta,now_ms,running,disk_bytes,bytes_in,bytes_out,incarnation):
        """Checkpoint and debit in one transaction; same checkpoint never charged twice.

        A new worker incarnation does not invent ON usage during an outage.
        Persistent counters are cumulative per VM and independent of guest data.
        """
        vm,project,owner=meta['vm_id'],meta['project_id'],meta['owner_did']
        with self.db() as db:
            self.ensure(db,project,owner)
            mode,tariff=self.tariff(db)
            row=db.execute('SELECT payload FROM samples WHERE vm=?',(vm,)).fetchone()
            old=json.loads(row[0]) if row else None
            sample={'at':now_ms,'running':running,'disk':disk_bytes,'in':bytes_in,'out':bytes_out,
                    'vcpus':meta['vcpus'],'memory_mib':meta['memory_mib'],
                    'mode':mode,'tariff':tariff,'incarnation':incarnation,
                    'state':meta.get('state','running' if running else 'stopped'),
                    'execution_mode':meta.get('execution_mode','serverless')}
            if old and now_ms<old['at']: return
            if old and now_ms==old['at'] and all(old.get(k)==v for k,v in sample.items()): return
            if old:
                elapsed=now_ms-old['at']
                on=elapsed if old['running'] and old['incarnation']==incarnation else 0
                usage={'vcpu_ms':old['vcpus']*on,'ram_byte_ms':old['memory_mib']*1024**2*on,
                       'disk_byte_ms':old['disk']*elapsed,'bytes_in':max(0,bytes_in-old['in']),
                       'bytes_out':max(0,bytes_out-old['out'])}
                old.setdefault('state','running' if old['running'] else 'stopped')
                period_id=old.get('period_id',f'period:{vm}:{old["at"]}')
                self._charge(db,project,owner,period_id,usage,old['mode'],old['tariff'],
                             {'vm_id':vm,'state':old['state'],'started_at':old['at']/1000,
                              'ended_at':now_ms/1000,'historical':False})
                fields=('state','running','vcpus','memory_mib','mode','tariff','execution_mode')
                if all(old.get(k)==sample.get(k) for k in fields): sample['period_id']=period_id
            sample.setdefault('period_id',f'period:{vm}:{now_ms}:{uuid.uuid4().hex}')
            db.execute('INSERT OR REPLACE INTO samples VALUES(?,?)',(vm,json.dumps(sample)))

    def view(self,project,owner,include_entries=True):
        with self.db() as db:
            account=dict(self.ensure(db,project,owner))
            mode,tariff=self.tariff(db)
            depleted=mode=='enforced' and account['balance']==0
            budget_blocked=account['budget'] is not None and account['budget_spent']>=account['budget']
            alerts=[]
            if depleted: alerts.append('credits_exhausted')
            if budget_blocked: alerts.append('budget_exhausted')
            elif account['budget'] and account['budget_spent']*5>=account['budget']*4: alerts.append('budget_80_percent')
            recent=[dict(json.loads(r['payload']),entry_id=r['id'],created_at=r['created'],operation=r['operation']) for r in db.execute('SELECT id,created,operation,payload FROM entries WHERE project=? ORDER BY id DESC LIMIT 100',(project,))] if include_entries else []
            return {'project_id':project,'balance_microcredits':account['balance'],'spent_microcredits':account['spent'],
                    'estimated_microcredits':account['estimated'],'budget_microcredits':account['budget'],
                    'budget_spent_microcredits':account['budget_spent'],'budget_epoch':account['budget_epoch'],
                    'exhausted_at':account['exhausted_at'],
                    'delete_after':account['exhausted_at']+RETENTION_SECONDS if account['exhausted_at'] is not None else None,
                    'deletion_committed':bool(account['retention_claim']),
                    'execution_allowed':not depleted and not budget_blocked and not account['retention_claim'],
                    'alerts':alerts,'billing_mode':mode,'tariff':tariff,'entries':recent,
                    'microcredits_per_credit':1_000_000}

    def claim_expired(self,project,owner,vm_id):
        """Atomic boundary with concurrent top-ups; once claimed, deletion wins."""
        with self.db() as db:
            account=self.ensure(db,project,owner)
            mode,_=self.tariff(db)
            if account['retention_claim']: return account['retention_claim']==vm_id
            if (mode!='enforced' or account['balance'] or account['exhausted_at'] is None
                    or self.clock()<account['exhausted_at']+RETENTION_SECONDS): return False
            db.execute('UPDATE accounts SET retention_claim=? WHERE project=?',(vm_id,project))
            return True

    def finish_deletion(self,project,owner,vm_id):
        with self.db() as db:
            account=self.ensure(db,project,owner)
            if account['retention_claim']!=vm_id: raise BillingError('invalid_deletion_claim')
            db.execute('UPDATE accounts SET retention_claim=NULL,exhausted_at=NULL WHERE project=?',(project,))
            key='delete:'+vm_id
            digest,old=self.operation(db,project,key,{'vm_id':vm_id})
            if old is None: self.entry(db,project,key,digest,{'kind':'storage_deleted','vm_id':vm_id})
