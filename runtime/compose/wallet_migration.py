"""Explicit, irreversible source fence for wallets with no retained VM assets."""
import hashlib
import json
import uuid

from billing import BillingError


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), allow_nan=False)


def schema(db):
    db.execute('''CREATE TABLE IF NOT EXISTS wallet_migrations(project TEXT PRIMARY KEY,
        owner TEXT NOT NULL,operator TEXT NOT NULL,node TEXT NOT NULL,transfer TEXT NOT NULL,
        snapshot TEXT NOT NULL,digest TEXT NOT NULL,state TEXT NOT NULL,receipt TEXT)''')
    columns={r[1] for r in db.execute('PRAGMA table_info(fleet_bindings)')}
    for key in ('legacy_spent','legacy_estimated'):
        if key not in columns:db.execute('ALTER TABLE fleet_bindings ADD COLUMN '+key+' INTEGER NOT NULL DEFAULT 0')


def view(row):
    return dict(project_id=row['project'],owner_did=row['owner'],operator_id=row['operator'],node_id=row['node'],
                transfer_id=row['transfer'],snapshot=json.loads(row['snapshot']),snapshot_digest=row['digest'],
                state=row['state'],source_fenced=True)


def status(ledger,project,owner):
    with ledger.db() as db:
        row=db.execute('SELECT * FROM wallet_migrations WHERE project=? AND owner=?',(project,owner)).fetchone()
        if not row:raise BillingError('wallet_migration_not_prepared')
        return view(row)


def prepare(ledger,project,owner,dry_run=False):
    config=getattr(ledger,'config',None)
    if not config:raise BillingError('fleet_configuration_required')
    with ledger.db() as db:
        old=db.execute('SELECT * FROM wallet_migrations WHERE project=?',(project,)).fetchone()
        if old:
            if (old['owner'],old['operator'],old['node'])!=(owner,config['operator_id'],config['node_id']):
                raise BillingError('wallet_migration_binding_conflict')
            return view(old)
        if project in ledger.projects or db.execute('SELECT 1 FROM fleet_bindings WHERE project=?',(project,)).fetchone():
            raise BillingError('wallet_already_managed')
        account=ledger.ensure(db,project,owner)
        mode,tariff=ledger.tariff(db)
        if mode!='enforced' or not tariff:raise BillingError('fleet_requires_enforced_billing')
        if account['retention_claim']:raise BillingError('storage_deletion_already_committed')
        # Do not guess which historical estimates were shadow usage or debt.
        if account['estimated']!=account['spent']:raise BillingError('legacy_metering_reconciliation_required')
        snapshot={k:account[k] for k in ('balance','spent','estimated','remainder','shadow_remainder',
                    'budget','budget_spent','budget_epoch')}
        snapshot.update(mode=mode,tariff_version=tariff['version'])
        text=encoded(snapshot);hashed=hashlib.sha256(text.encode()).hexdigest()
        if dry_run:return dict(snapshot=snapshot,snapshot_digest=hashed)
        db.execute('INSERT INTO wallet_migrations VALUES(?,?,?,?,?,?,?,?,NULL)',
            (project,owner,config['operator_id'],config['node_id'],'mig_'+uuid.uuid4().hex,text,hashed,'fenced'))
        result=view(db.execute('SELECT * FROM wallet_migrations WHERE project=?',(project,)).fetchone())
    ledger.fenced_projects.add(project)
    return result


def commit(ledger,project,owner):
    saved=status(ledger,project,owner)
    config=getattr(ledger,'config',None)
    if not config or (saved['operator_id'],saved['node_id'])!=(config['operator_id'],config['node_id']):
        raise BillingError('wallet_migration_binding_conflict')
    if saved['state']=='committed':return saved
    body={k:saved[k] for k in ('project_id','owner_did','transfer_id','snapshot_digest')}
    body.update(action='wallet-import',request_id=saved['transfer_id'])
    reply=ledger.transport(body)
    expected={k:saved[k] for k in ('operator_id','node_id','project_id','owner_did','transfer_id','snapshot_digest')}
    expected.update(state='credited',credited_microcredits=saved['snapshot']['balance'])
    if any(reply.get(k)!=v for k,v in expected.items()):raise BillingError('wallet_migration_invalid_receipt')
    with ledger.db() as db:
        row=db.execute('SELECT * FROM wallet_migrations WHERE project=?',(project,)).fetchone()
        if row['state']=='committed':return view(row)
        account=db.execute('SELECT * FROM accounts WHERE project=?',(project,)).fetchone()
        if any(account[k]!=v for k,v in saved['snapshot'].items() if k not in ('mode','tariff_version')):
            raise BillingError('wallet_migration_source_changed')
        db.execute('INSERT INTO fleet_bindings(project,owner,reservation,operator,node,legacy_spent,legacy_estimated) VALUES(?,?,?,?,?,?,?)',
            (project,owner,'rsv_'+uuid.uuid4().hex,config['operator_id'],config['node_id'],account['spent'],account['estimated']))
        db.execute('UPDATE accounts SET balance=0,exhausted_at=NULL WHERE project=?',(project,))
        payload=dict(kind='fleet_transfer',amount_microcredits=saved['snapshot']['balance'],transfer_id=saved['transfer_id'],direction='out')
        ledger.entry(db,project,'fleet-transfer:'+saved['transfer_id'],saved['snapshot_digest'],payload)
        db.execute("UPDATE wallet_migrations SET state='committed',receipt=? WHERE project=?",(encoded(reply),project))
        result=view(db.execute('SELECT * FROM wallet_migrations WHERE project=?',(project,)).fetchone())
    ledger.fenced_projects.add(project)
    return result


def operate(runner,action,project,owner):
    ledger=runner.runtime.ledger
    if action=='wallet-migration-status':return status(ledger,project,owner)
    runner.authorize(project,owner)
    with runner.runtime.lock(project),runner.hypervisor.owner_lock(owner):
        metas=runner.hypervisor.list(project,owner)
        # This first migration path intentionally refuses live/stopped/hibernated
        # VMs and retained disks. It never stops or deletes a user's workload.
        if any(m['state']!='destroyed' or runner.hypervisor.alive(m) or runner.runtime.storage_bytes(m) for m in metas):
            raise BillingError('wallet_migration_requires_no_vm_assets')
        if action=='prepare-wallet-migration':
            config=getattr(ledger,'config',None)
            if not config:raise BillingError('fleet_configuration_required')
            prepare(ledger,project,owner,dry_run=True)
            # Capacity fence precedes the money fence. A crash between them
            # blocks future VM creation but has not exported any spendable money.
            runner.hypervisor.capacity.fence_legacy_wallet(project,owner,config)
            return prepare(ledger,project,owner)
        return commit(ledger,project,owner)
