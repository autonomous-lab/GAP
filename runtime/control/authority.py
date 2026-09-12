"""Single-writer operator account registry and integer-microcredit wallet.

This is the control-plane foundation, not a replacement for node authentication
or worker metering. Import staging never makes a legacy balance spendable.
"""
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import sqlite3
import time

MAX_CREDITS = 10**15
DEFAULT_QUOTAS = dict(max_vms=1, cpu_quarters=4, memory_mib=1024)
MAX_QUOTAS = dict(max_vms=1000000, cpu_quarters=4000000, memory_mib=2**40)


class Failure(ValueError):
    def __init__(self, code, status=400):
        self.code, self.status = code, status
        super().__init__(code)


def identifier(value):
    if not isinstance(value, str) or not re.fullmatch(r'[A-Za-z0-9_.:-]{1,128}', value):
        raise Failure('invalid_identifier')
    return value


def amount(value, minimum=0):
    if type(value) is not int or not minimum <= value <= MAX_CREDITS:
        raise Failure('invalid_microcredits')
    return value


def encode(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), allow_nan=False)


def digest(value):
    return hashlib.sha256(encode(value).encode()).hexdigest()


class Authority:
    def __init__(self, path, operator, clock=time.time):
        self.path, self.operator, self.clock = str(path), identifier(operator), clock
        # Create privately before SQLite opens it, including on first startup.
        fd = os.open(self.path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
        os.close(fd)
        Path(self.path).chmod(0o600)
        with self.db() as db:
            for sql in (
                'CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL)',
                '''CREATE TABLE IF NOT EXISTS customers(id TEXT PRIMARY KEY, label TEXT NOT NULL,
                   balance INTEGER NOT NULL DEFAULT 0 CHECK(balance>=0), spent INTEGER NOT NULL DEFAULT 0,
                   created INTEGER NOT NULL)''',
                '''CREATE TABLE IF NOT EXISTS principals(kind TEXT NOT NULL,subject TEXT NOT NULL,
                   customer TEXT NOT NULL REFERENCES customers(id), verified INTEGER NOT NULL,
                   PRIMARY KEY(kind,subject))''',
                '''CREATE TABLE IF NOT EXISTS projects(id TEXT PRIMARY KEY,customer TEXT NOT NULL REFERENCES customers(id),
                   node TEXT NOT NULL,owner TEXT NOT NULL)''',
                '''CREATE TABLE IF NOT EXISTS grants(project TEXT NOT NULL REFERENCES projects(id),
                   agent TEXT NOT NULL,role TEXT NOT NULL,PRIMARY KEY(project,agent))''',
                '''CREATE TABLE IF NOT EXISTS operations(actor TEXT NOT NULL,id TEXT NOT NULL,
                   digest TEXT NOT NULL,result TEXT NOT NULL,created INTEGER NOT NULL,PRIMARY KEY(actor,id))''',
                '''CREATE TABLE IF NOT EXISTS wallet_entries(id INTEGER PRIMARY KEY AUTOINCREMENT,
                   customer TEXT NOT NULL REFERENCES customers(id),project TEXT, node TEXT,kind TEXT NOT NULL,
                   delta INTEGER NOT NULL,source TEXT NOT NULL,actor TEXT NOT NULL,operation TEXT NOT NULL,created INTEGER NOT NULL)''',
                '''CREATE TABLE IF NOT EXISTS imports(source TEXT NOT NULL,project TEXT NOT NULL,
                   customer TEXT NOT NULL REFERENCES customers(id),digest TEXT NOT NULL,payload TEXT NOT NULL,
                   state TEXT NOT NULL DEFAULT 'staged',PRIMARY KEY(source,project))''',
                '''CREATE TABLE IF NOT EXISTS credentials(hash TEXT PRIMARY KEY,customer TEXT NOT NULL REFERENCES customers(id),
                   agent TEXT,expires INTEGER NOT NULL,revoked INTEGER NOT NULL DEFAULT 0)''',
                '''CREATE TABLE IF NOT EXISTS reservations(id TEXT PRIMARY KEY,node TEXT NOT NULL,
                   project TEXT NOT NULL REFERENCES projects(id), customer TEXT NOT NULL REFERENCES customers(id),
                   allocated INTEGER NOT NULL DEFAULT 0, consumed INTEGER NOT NULL DEFAULT 0,
                   unpaid INTEGER NOT NULL DEFAULT 0, expires INTEGER NOT NULL DEFAULT 0,
                   closed INTEGER NOT NULL DEFAULT 0)''',
                'CREATE UNIQUE INDEX IF NOT EXISTS active_reservation ON reservations(node,project) WHERE closed=0',
                '''CREATE TABLE IF NOT EXISTS quotas(customer TEXT PRIMARY KEY REFERENCES customers(id),
                   max_vms INTEGER NOT NULL, cpu_quarters INTEGER NOT NULL, memory_mib INTEGER NOT NULL,
                   revision INTEGER NOT NULL)''',
                '''CREATE TABLE IF NOT EXISTS capacity(vm TEXT PRIMARY KEY, node TEXT NOT NULL,
                   project TEXT NOT NULL REFERENCES projects(id), customer TEXT NOT NULL REFERENCES customers(id),
                   state TEXT NOT NULL, revision INTEGER NOT NULL, cpu INTEGER NOT NULL, memory INTEGER NOT NULL,
                   target_cpu INTEGER NOT NULL, target_memory INTEGER NOT NULL, pending TEXT)''',
                'CREATE INDEX IF NOT EXISTS capacity_customer ON capacity(customer,state,vm)',
            ):
                db.execute(sql)
            db.execute("INSERT OR IGNORE INTO metadata VALUES('operator',?)", (operator,))
            from wallet_import import schema
            schema(db)
            if db.execute("SELECT value FROM metadata WHERE key='operator'").fetchone()[0] != operator:
                raise Failure('operator_database_mismatch', 409)

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.path, timeout=5, isolation_level=None)
        db.row_factory = sqlite3.Row
        try:
            db.execute('PRAGMA foreign_keys=ON')
            db.execute('PRAGMA journal_mode=WAL')
            db.execute('PRAGMA synchronous=FULL')
            db.execute('BEGIN IMMEDIATE')
            yield db
            db.execute('COMMIT')
        except BaseException:
            if db.in_transaction:
                db.execute('ROLLBACK')
            raise
        finally:
            db.close()

    def mutation(self, actor, request, body, action):
        identifier(actor)
        identifier(request)
        hashed = digest(body)
        with self.db() as db:
            previous = db.execute('SELECT digest,result FROM operations WHERE actor=? AND id=?', (actor, request)).fetchone()
            if previous:
                if previous['digest'] != hashed:
                    raise Failure('request_id_conflict', 409)
                return json.loads(previous['result'])
            result = action(db)
            db.execute('INSERT INTO operations VALUES(?,?,?,?,?)', (actor, request, hashed, encode(result), int(self.clock())))
            return result

    @staticmethod
    def customer(db, customer):
        identifier(customer)
        row = db.execute('SELECT * FROM customers WHERE id=?', (customer,)).fetchone()
        if not row:
            raise Failure('unknown_customer', 404)
        return row

    def create_customer(self, actor, request, label):
        if not isinstance(label, str) or not 1 <= len(label) <= 200 or any(ord(c) < 32 for c in label):
            raise Failure('invalid_customer_label')
        def create(db):
            customer = 'cus_' + secrets.token_hex(16)
            db.execute('INSERT INTO customers(id,label,created) VALUES(?,?,?)', (customer, label, int(self.clock())))
            return {'operator_id': self.operator, 'customer_id': customer}
        return self.mutation(actor, request, {'action': 'create_customer', 'label': label}, create)

    def attach_principal(self, actor, request, customer, kind, subject, verified=False):
        if kind == 'agent':
            if not isinstance(subject, str) or not re.fullmatch(r'did:gap:[0-9a-f]{64}', subject) or verified is not False:
                raise Failure('invalid_agent')
        elif kind == 'human':
            # The human ID is opaque. Email linking and proof remain in the
            # verified-registration service; importing a DID proves no email.
            identifier(subject)
            if type(verified) is not bool:
                raise Failure('invalid_verification_status')
        else:
            raise Failure('invalid_principal_kind')
        body = dict(action='attach_principal', customer=customer, kind=kind, subject=subject, verified=verified)
        def attach(db):
            self.customer(db, customer)
            old = db.execute('SELECT * FROM principals WHERE kind=? AND subject=?', (kind, subject)).fetchone()
            if old and (old['customer'] != customer or bool(old['verified']) != verified):
                raise Failure('principal_binding_conflict', 409)
            db.execute('INSERT OR IGNORE INTO principals VALUES(?,?,?,?)', (kind, subject, customer, int(verified)))
            return body
        return self.mutation(actor, request, body, attach)

    def attach_project(self, actor, request, customer, project, node, owner):
        if not isinstance(project, str) or not re.fullmatch(r'prj_[0-9a-f]{24}', project):
            raise Failure('invalid_project')
        identifier(node)
        body = dict(action='attach_project', customer=customer, project=project, node=node, owner=owner)
        def attach(db):
            self.customer(db, customer)
            principal = db.execute("SELECT customer FROM principals WHERE kind='agent' AND subject=?", (owner,)).fetchone()
            if not principal or principal[0] != customer:
                raise Failure('owner_membership_required', 403)
            old = db.execute('SELECT * FROM projects WHERE id=?', (project,)).fetchone()
            if old and (old['customer'], old['node'], old['owner']) != (customer, node, owner):
                raise Failure('project_binding_conflict', 409)
            db.execute('INSERT OR IGNORE INTO projects VALUES(?,?,?,?)', (project, customer, node, owner))
            db.execute("INSERT OR IGNORE INTO grants VALUES(?,?,'owner')", (project, owner))
            return body
        return self.mutation(actor, request, body, attach)

    def grant(self, actor, request, project, agent, role):
        if role not in ('viewer', 'operator', 'none'):
            raise Failure('invalid_project_role')
        body = dict(action='grant', project=project, agent=agent, role=role)
        def update(db):
            row = db.execute('SELECT * FROM projects WHERE id=?', (project,)).fetchone()
            member = db.execute("SELECT customer FROM principals WHERE kind='agent' AND subject=?", (agent,)).fetchone()
            if not row or not member or row['customer'] != member[0]:
                raise Failure('project_membership_required', 403)
            if row['owner'] == agent:
                raise Failure('owner_grant_immutable', 409)
            db.execute('DELETE FROM grants WHERE project=? AND agent=?', (project, agent))
            if role != 'none':
                db.execute('INSERT INTO grants VALUES(?,?,?)', (project, agent, role))
            return body
        return self.mutation(actor, request, body, update)

    @staticmethod
    def node_project(db, node, project):
        row = db.execute('SELECT * FROM projects WHERE id=? AND node=?', (project, node)).fetchone()
        if not row:
            raise Failure('project_node_mismatch', 403)
        return row

    @staticmethod
    def capacity_number(value, maximum, minimum=0):
        if type(value) is not int or not minimum <= value <= maximum:
            raise Failure('invalid_capacity_value')
        return value

    def quota_state(self, db, customer):
        self.customer(db, customer)
        row = db.execute('SELECT * FROM quotas WHERE customer=?', (customer,)).fetchone()
        limits = {k: row[k] for k in DEFAULT_QUOTAS} if row else dict(DEFAULT_QUOTAS)
        # A pending resize holds the componentwise maximum of old and new
        # allocations. Never lend out a reduction before the worker applies it.
        used = db.execute('''SELECT count(*) AS max_vms,
            coalesce(sum(max(cpu,target_cpu)),0) AS cpu_quarters,
            coalesce(sum(max(memory,target_memory)),0) AS memory_mib
            FROM capacity WHERE customer=? AND state!='released' ''', (customer,)).fetchone()
        return dict(operator_id=self.operator, customer_id=customer, limits=limits,
                    allocated=dict(used), revision=row['revision'] if row else 0,
                    over_limit=[k for k in limits if used[k] > limits[k]])

    def quotas(self, customer):
        with self.db() as db:
            return self.quota_state(db, customer)

    def set_quotas(self, actor, request, customer, limits, expected_revision):
        self.capacity_number(expected_revision, 2**53-1)
        if not isinstance(limits, dict) or set(limits) != set(DEFAULT_QUOTAS):
            raise Failure('invalid_capacity_limits')
        for key, value in limits.items():
            self.capacity_number(value, MAX_QUOTAS[key])
        body = dict(action='set_quotas', customer=customer, limits=limits, expected_revision=expected_revision)
        def apply(db):
            current = self.quota_state(db, customer)
            if current['revision'] != expected_revision:
                raise Failure('quota_revision_conflict', 409)
            db.execute('''INSERT INTO quotas VALUES(?,?,?,?,?) ON CONFLICT(customer) DO UPDATE SET
                max_vms=excluded.max_vms,cpu_quarters=excluded.cpu_quarters,
                memory_mib=excluded.memory_mib,revision=excluded.revision''',
                (customer, limits['max_vms'], limits['cpu_quarters'], limits['memory_mib'], expected_revision+1))
            return self.quota_state(db, customer)
        return self.mutation(actor, request, body, apply)

    @staticmethod
    def capacity_view(row):
        return dict(vm_id=row['vm'], node_id=row['node'], project_id=row['project'], customer_id=row['customer'],
                    state=row['state'], revision=row['revision'], transition_id=row['pending'],
                    committed=dict(cpu_quarters=row['cpu'], memory_mib=row['memory']),
                    target=dict(cpu_quarters=row['target_cpu'], memory_mib=row['target_memory']))

    def capacity_row(self, db, node, project, vm):
        self.node_project(db, node, project)
        if not isinstance(vm, str) or not re.fullmatch(r'vm_[0-9a-f]{32}', vm):
            raise Failure('invalid_vm_id')
        row = db.execute('SELECT * FROM capacity WHERE vm=?', (vm,)).fetchone()
        if row and (row['node'], row['project']) != (node, project):
            raise Failure('capacity_binding_mismatch', 403)
        return row

    def capacity_get(self, node, project, vm):
        with self.db() as db:
            row = self.capacity_row(db, node, project, vm)
            if not row:
                raise Failure('capacity_not_found', 404)
            return dict(operator_id=self.operator, **self.capacity_view(row))

    def capacity_list(self, customer, after=''):
        if not isinstance(after, str) or len(after) > 128:
            raise Failure('invalid_capacity_cursor')
        with self.db() as db:
            self.customer(db, customer)
            rows = db.execute('SELECT * FROM capacity WHERE customer=? AND vm>? ORDER BY vm LIMIT 101',
                              (customer, after)).fetchall()
            return dict(operator_id=self.operator, customer_id=customer,
                        allocations=[self.capacity_view(r) for r in rows[:100]],
                        next_cursor=rows[99]['vm'] if len(rows) > 100 else None)

    def capacity_prepare(self, node, request, project, owner, vm, cpu, memory, expected_revision):
        self.capacity_number(cpu, MAX_QUOTAS['cpu_quarters'], 1)
        self.capacity_number(memory, MAX_QUOTAS['memory_mib'], 256)
        self.capacity_number(expected_revision, 2**53-1)
        body = dict(action='capacity_prepare', project=project, owner=owner, vm=vm,
                    cpu=cpu, memory=memory, expected_revision=expected_revision)
        def apply(db):
            placement = self.node_project(db, node, project)
            if placement['owner'] != owner:
                raise Failure('project_owner_mismatch', 403)
            row = self.capacity_row(db, node, project, vm)
            if row and row['state'] == 'released':
                raise Failure('capacity_released', 409)
            if (row['revision'] if row else 0) != expected_revision:
                raise Failure('capacity_revision_conflict', 409)
            if row and row['state'] != 'active':
                raise Failure('capacity_transition_pending', 409)
            current = self.quota_state(db, placement['customer'])
            growth = dict(max_vms=0 if row else 1,
                          cpu_quarters=max(0, cpu-(row['cpu'] if row else 0)),
                          memory_mib=max(0, memory-(row['memory'] if row else 0)))
            for key, delta in growth.items():
                # Lowered quotas do not kill existing VMs or block reductions.
                if delta and current['allocated'][key]+delta > current['limits'][key]:
                    raise Failure('customer_quota_exceeded_'+key, 409)
            if row:
                db.execute('''UPDATE capacity SET state='pending',revision=revision+1,
                    target_cpu=?,target_memory=?,pending=? WHERE vm=?''', (cpu,memory,request,vm))
            else:
                db.execute("INSERT INTO capacity VALUES(?,?,?,?,'pending',1,0,0,?,?,?)",
                           (vm,node,project,placement['customer'],cpu,memory,request))
            return dict(operator_id=self.operator, **self.capacity_view(self.capacity_row(db,node,project,vm)))
        return self.mutation('node:'+node, request, body, apply)

    def capacity_finish(self, node, request, project, vm, expected_revision, transition, outcome, evidence):
        """A trusted node asserts a DURABLE local result, not a timeout guess.

        Evidence is an audit receipt ID, not cryptographic proof of host state.
        No operator force-release or expiry can bypass reconciliation here.
        """
        self.capacity_number(expected_revision, 2**53-1, 1)
        identifier(evidence)
        if outcome not in ('commit', 'abort', 'release'):
            raise Failure('invalid_capacity_outcome')
        if outcome != 'release':
            identifier(transition)
        elif transition is not None:
            raise Failure('invalid_capacity_transition')
        body = dict(action='capacity_finish', project=project, vm=vm, expected_revision=expected_revision,
                    transition=transition, outcome=outcome, evidence=evidence)
        def apply(db):
            row = self.capacity_row(db, node, project, vm)
            if not row:
                raise Failure('capacity_not_found', 404)
            if row['revision'] != expected_revision:
                raise Failure('capacity_revision_conflict', 409)
            if row['state'] == 'released':
                raise Failure('capacity_released', 409)
            if outcome == 'release':
                if row['state'] != 'active':
                    raise Failure('capacity_transition_pending', 409)
                cpu, memory, state = 0, 0, 'released'
            else:
                if row['state'] != 'pending' or row['pending'] != transition:
                    raise Failure('capacity_transition_mismatch', 409)
                cpu, memory = (row['target_cpu'], row['target_memory']) if outcome == 'commit' else (row['cpu'],row['memory'])
                state = 'active' if cpu else 'released'
            db.execute('''UPDATE capacity SET state=?,revision=revision+1,cpu=?,memory=?,
                target_cpu=?,target_memory=?,pending=NULL WHERE vm=?''', (state,cpu,memory,cpu,memory,vm))
            return dict(operator_id=self.operator, evidence_id=evidence, outcome=outcome,
                        **self.capacity_view(self.capacity_row(db,node,project,vm)))
        return self.mutation('node:'+node, request, body, apply)

    def capacity_cancel_create(self, node, request, project, owner, vm, evidence):
        """Fence an abandoned creation even when its prepare has not arrived yet."""
        identifier(evidence)
        body = dict(action='capacity_cancel_create',project=project,owner=owner,vm=vm,evidence=evidence)
        def apply(db):
            placement=self.node_project(db,node,project)
            if placement['owner']!=owner: raise Failure('project_owner_mismatch',403)
            row=self.capacity_row(db,node,project,vm)
            if row and row['state']=='active': raise Failure('capacity_already_committed',409)
            if row and row['cpu']: raise Failure('capacity_resize_cannot_cancel_creation',409)
            if row:
                if row['state']!='released':
                    db.execute("UPDATE capacity SET state='released',revision=revision+1,target_cpu=0,target_memory=0,pending=NULL WHERE vm=?",(vm,))
            else:
                db.execute("INSERT INTO capacity VALUES(?,?,?,?,'released',1,0,0,0,0,NULL)",
                           (vm,node,project,placement['customer']))
            return dict(operator_id=self.operator,evidence_id=evidence,
                        **self.capacity_view(self.capacity_row(db,node,project,vm)))
        return self.mutation('node:'+node,request,body,apply)

    def capacity_abort_resize(self,node,request,project,vm,revision,transition,evidence):
        self.capacity_number(revision,2**53-1,1);identifier(transition);identifier(evidence)
        body=dict(action='capacity_abort_resize',project=project,vm=vm,revision=revision,transition=transition,evidence=evidence)
        def apply(db):
            row=self.capacity_row(db,node,project,vm)
            if not row:raise Failure('capacity_not_found',404)
            unreceived=row['state']=='active' and row['revision']==revision
            pending=row['state']=='pending' and row['revision']==revision+1 and row['pending']==transition and row['cpu']>0
            if not (unreceived or pending):raise Failure('capacity_revision_conflict',409)
            db.execute("UPDATE capacity SET state='active',revision=revision+1,target_cpu=cpu,target_memory=memory,pending=NULL WHERE vm=?",(vm,))
            return dict(operator_id=self.operator,evidence_id=evidence,**self.capacity_view(self.capacity_row(db,node,project,vm)))
        return self.mutation('node:'+node,request,body,apply)

    def entry(self, db, customer, project, node, kind, delta, source, actor, request):
        db.execute('INSERT INTO wallet_entries(customer,project,node,kind,delta,source,actor,operation,created) VALUES(?,?,?,?,?,?,?,?,?)',
                   (customer, project, node, kind, delta, source, actor, request, int(self.clock())))

    def topup(self, actor, request, customer, value, source):
        amount(value, 1)
        if source not in ('paid', 'promotional', 'unclassified'):
            raise Failure('invalid_funding_source')
        body = dict(action='topup', customer=customer, amount=value, source=source)
        def apply(db):
            row = self.customer(db, customer)
            held = db.execute('SELECT coalesce(sum(allocated-consumed),0) FROM reservations WHERE customer=? AND closed=0',(customer,)).fetchone()[0]
            amount(row['balance']+held+value)
            balance = amount(row['balance'] + value)
            db.execute('UPDATE customers SET balance=? WHERE id=?', (balance, customer))
            self.entry(db, customer, None, None, 'funding', value, source, actor, request)
            return dict(customer_id=customer, balance_microcredits=balance)
        return self.mutation(actor, request, body, apply)

    def debit(self, node, request, project, value):
        """Direct online primitive; workers use checkpoint reservations instead.

        Reject insufficient funds as a whole; a caller must never interpret a
        transport failure as zero credit or silently retry under a new ID.
        """
        amount(value, 1)
        body = dict(action='debit', project=project, amount=value)
        def apply(db):
            placement = self.node_project(db, node, project)
            customer = self.customer(db, placement['customer'])
            if customer['balance'] < value:
                raise Failure('insufficient_credits', 402)
            amount(customer['spent'] + value)
            balance = customer['balance'] - value
            db.execute('UPDATE customers SET balance=?,spent=spent+? WHERE id=?', (balance, value, customer['id']))
            self.entry(db, customer['id'], project, node, 'usage', -value, 'metered', 'node:' + node, request)
            return dict(customer_id=customer['id'], balance_microcredits=balance, debited_microcredits=value)
        return self.mutation('node:' + node, request, body, apply)

    def stage_import(self, actor, request, customer, source, project, snapshot):
        """Immutable inventory only: no credit increase, activation or source writes.

        A separate cutover must fence the source and verify its final checkpoint.
        There deliberately is no API to mark a staged balance spendable yet.
        """
        identifier(source)
        expected = {'owner_did', 'balance', 'spent', 'remainder', 'shadow_remainder', 'budget',
                    'budget_spent', 'budget_epoch', 'exhausted_at', 'retention_claim'}
        if not isinstance(snapshot, dict) or set(snapshot) != expected:
            raise Failure('invalid_legacy_snapshot')
        for key in ('balance', 'spent', 'budget_spent', 'budget_epoch'):
            amount(snapshot[key])
        if snapshot['budget'] is not None:
            amount(snapshot['budget'], 1)
        # Validate rational carries exactly, never convert through float.
        from fractions import Fraction
        for key in ('remainder', 'shadow_remainder'):
            if not isinstance(snapshot[key], (str, int)) or isinstance(snapshot[key], bool) or len(str(snapshot[key])) > 128:
                raise Failure('invalid_fractional_carry')
            try:
                if not 0 <= Fraction(snapshot[key]) < 1024**3 * 3_600_000:
                    raise ValueError()
            except (ValueError, ZeroDivisionError):
                raise Failure('invalid_fractional_carry') from None
        if snapshot['exhausted_at'] is not None:
            import math
            value = snapshot['exhausted_at']
            if type(value) not in (int, float) or not math.isfinite(value) or value < 0:
                raise Failure('invalid_exhaustion_time')
        if snapshot['retention_claim'] is not None:
            identifier(snapshot['retention_claim'])
        body = dict(action='stage_import', customer=customer, source=source, project=project, snapshot=snapshot)
        def stage(db):
            placement = self.node_project(db, source, project)
            if placement['customer'] != customer or placement['owner'] != snapshot['owner_did']:
                raise Failure('legacy_owner_mismatch', 409)
            hashed = digest(snapshot)
            row = db.execute('SELECT digest FROM imports WHERE source=? AND project=?', (source, project)).fetchone()
            if row and row[0] != hashed:
                raise Failure('legacy_snapshot_changed', 409)
            db.execute('INSERT OR IGNORE INTO imports(source,project,customer,digest,payload) VALUES(?,?,?,?,?)',
                       (source, project, customer, hashed, encode(snapshot)))
            return dict(state='staged', spendable=False, source=source, project=project, snapshot_digest=hashed)
        return self.mutation(actor, request, body, stage)

    def wallet(self, customer):
        with self.db() as db:
            row = self.customer(db, customer)
            pending = list(db.execute("SELECT source,project,payload FROM imports i WHERE customer=? AND NOT EXISTS (SELECT 1 FROM wallet_imports m WHERE m.node=i.source AND m.project=i.project AND m.state='credited')", (customer,)))
            reserved = db.execute('SELECT coalesce(sum(allocated-consumed),0) FROM reservations WHERE customer=? AND closed=0', (customer,)).fetchone()[0]
            return dict(operator_id=self.operator, customer_id=customer, balance_microcredits=row['balance'],
                        reserved_microcredits=reserved, total_remaining_microcredits=row['balance']+reserved,
                        spent_microcredits=row['spent'], microcredits_per_credit=1_000_000,
                        staged_legacy_microcredits=sum(json.loads(r['payload'])['balance'] for r in pending),
                        staged_legacy_spendable=False)

    def checkpoint(self, node, request, project, owner, reservation, consumed, unpaid, target, lease_seconds, close=False):
        """Settle a durable cumulative checkpoint, then reserve/renew atomically.

        Expiry fences execution; it NEVER refunds an unknown node's allocation.
        Only an explicit final checkpoint can release unused reserved credits.
        The transport adds fresh authority time to every response, including a
        cached retry, so replay cannot manufacture another lease lifetime.
        """
        identifier(reservation)
        for value in (consumed, unpaid, target):
            amount(value)
        if target > 1_000_000 or type(lease_seconds) is not int or not 10 <= lease_seconds <= 60 or type(close) is not bool:
            raise Failure('invalid_reservation_limits')
        body = dict(action='checkpoint', project=project, owner=owner, reservation=reservation,
                    consumed=consumed, unpaid=unpaid, target=target, lease_seconds=lease_seconds, close=close)
        def apply(db):
            placement = self.node_project(db, node, project)
            if placement['owner'] != owner:
                raise Failure('reservation_owner_mismatch', 403)
            customer = self.customer(db, placement['customer'])
            old = db.execute('SELECT * FROM reservations WHERE id=?', (reservation,)).fetchone()
            if old is None:
                if consumed or unpaid or close:
                    raise Failure('unknown_reservation', 409)
                if db.execute('SELECT 1 FROM reservations WHERE node=? AND project=? AND closed=0', (node, project)).fetchone():
                    raise Failure('active_reservation_exists', 409)
                db.execute('INSERT INTO reservations(id,node,project,customer) VALUES(?,?,?,?)', (reservation,node,project,customer['id']))
                old = db.execute('SELECT * FROM reservations WHERE id=?', (reservation,)).fetchone()
            if (old['node'],old['project'],old['customer']) != (node,project,customer['id']):
                raise Failure('reservation_binding_mismatch', 403)
            if old['closed']:
                raise Failure('reservation_closed', 409)
            if not old['consumed'] <= consumed <= old['allocated']:
                raise Failure('invalid_consumption_checkpoint', 409)
            debit = consumed-old['consumed']
            amount(customer['spent']+debit)
            if debit:
                db.execute('UPDATE customers SET spent=spent+? WHERE id=?', (debit,customer['id']))
                self.entry(db,customer['id'],project,node,'usage',-debit,'reservation','node:'+node,request)
            held = old['allocated']-consumed
            if close:
                if unpaid:
                    raise Failure('unpaid_usage_requires_reconciliation', 409)
                db.execute('UPDATE customers SET balance=balance+? WHERE id=?', (held,customer['id']))
                allocated, expires = old['allocated'], 0
            else:
                extra = min(max(0,target-held),customer['balance'])
                allocated = amount(old['allocated']+extra)
                db.execute('UPDATE customers SET balance=balance-? WHERE id=?', (extra,customer['id']))
                expires = int(self.clock())+lease_seconds if held+extra else 0
            db.execute('UPDATE reservations SET allocated=?,consumed=?,unpaid=?,expires=?,closed=? WHERE id=?',
                       (allocated,consumed,unpaid,expires,int(close),reservation))
            remaining=db.execute('SELECT coalesce(sum(allocated-consumed),0) FROM reservations WHERE customer=? AND closed=0',(customer['id'],)).fetchone()[0]
            return dict(operator_id=self.operator,node_id=node,project_id=project,owner_did=owner,
                        reservation_id=reservation,allocated_microcredits=allocated,consumed_microcredits=consumed,
                        unpaid_microcredits=unpaid,lease_expires_at=expires,closed=close,
                        funding_status='available' if expires else 'fully_reserved' if remaining else 'exhausted')
        return self.mutation('node:'+node,request,body,apply)

    def projects(self, customer, agent=None, after=''):
        with self.db() as db:
            self.customer(db, customer)
            if agent is None:
                rows = db.execute("SELECT id,node,owner,'owner' AS role FROM projects WHERE customer=? AND id>? ORDER BY id LIMIT 101", (customer, after)).fetchall()
            else:
                rows = db.execute('SELECT p.id,p.node,p.owner,g.role FROM projects p JOIN grants g ON g.project=p.id WHERE p.customer=? AND g.agent=? AND p.id>? ORDER BY p.id LIMIT 101', (customer, agent, after)).fetchall()
            return dict(projects=[dict(r) for r in rows[:100]], next_cursor=rows[99]['id'] if len(rows) > 100 else None)

    def issue(self, customer, agent, ttl=3600):
        if type(ttl) is not int or not 60 <= ttl <= 3600:
            raise Failure('invalid_token_lifetime')
        token = 'gapc_' + secrets.token_urlsafe(32)
        expires = int(self.clock()) + ttl
        with self.db() as db:
            self.customer(db, customer)
            if agent is not None:
                row = db.execute("SELECT customer FROM principals WHERE kind='agent' AND subject=?", (agent,)).fetchone()
                if not row or row[0] != customer:
                    raise Failure('agent_membership_required', 403)
            db.execute('INSERT INTO credentials(hash,customer,agent,expires) VALUES(?,?,?,?)',
                       (hashlib.sha256(token.encode()).hexdigest(), customer, agent, expires))
        return dict(operator_id=self.operator, token=token, expires_at=expires, audience='control',
                    customer_id=customer, agent_did=agent)

    def authenticate(self, token):
        if not isinstance(token, str) or not re.fullmatch(r'gapc_[A-Za-z0-9_-]{43}', token):
            raise Failure('invalid_control_credentials', 401)
        with self.db() as db:
            row = db.execute('SELECT * FROM credentials WHERE hash=? AND revoked=0 AND expires>?',
                             (hashlib.sha256(token.encode()).hexdigest(), int(self.clock()))).fetchone()
            if not row:
                raise Failure('invalid_control_credentials', 401)
            return dict(customer=row['customer'], agent=row['agent'])

    def revoke(self, token):
        with self.db() as db:
            db.execute('UPDATE credentials SET revoked=1 WHERE hash=?', (hashlib.sha256(token.encode()).hexdigest(),))
        return {'revoked': True}
