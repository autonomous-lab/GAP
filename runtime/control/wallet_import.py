"""A worker cannot create money: exact legacy imports need operator approval."""
from fractions import Fraction
import re

from authority import Failure, amount, digest, encode, identifier


def schema(db):
    db.execute('''CREATE TABLE IF NOT EXISTS wallet_imports(node TEXT NOT NULL,project TEXT NOT NULL,
        customer TEXT NOT NULL REFERENCES customers(id),owner TEXT NOT NULL,transfer TEXT NOT NULL UNIQUE,
        snapshot TEXT NOT NULL,digest TEXT NOT NULL,state TEXT NOT NULL,
        PRIMARY KEY(node,project))''')


def validate(snapshot):
    keys={'balance','spent','estimated','remainder','shadow_remainder','budget','budget_spent','budget_epoch','mode','tariff_version'}
    if not isinstance(snapshot,dict) or set(snapshot)!=keys:raise Failure('invalid_migration_snapshot')
    for key in ('balance','spent','estimated','budget_spent','budget_epoch'):amount(snapshot[key])
    if snapshot['budget'] is not None:amount(snapshot['budget'],1)
    if snapshot['estimated']!=snapshot['spent'] or snapshot['mode']!='enforced':
        raise Failure('legacy_metering_reconciliation_required',409)
    identifier(snapshot['tariff_version'])
    for key in ('remainder','shadow_remainder'):
        value=snapshot[key]
        if type(value) not in (str,int) or len(str(value))>128:raise Failure('invalid_fractional_carry')
        try:
            if not 0<=Fraction(value)<1024**3*3600000:raise ValueError()
        except (ValueError,ZeroDivisionError):raise Failure('invalid_fractional_carry') from None


def authorize(a,request,node,project,owner,transfer,snapshot,snapshot_digest):
    validate(snapshot)
    if not isinstance(transfer,str) or not re.fullmatch(r'mig_[0-9a-f]{32}',transfer):
        raise Failure('invalid_transfer_identity')
    if digest(snapshot)!=snapshot_digest:raise Failure('migration_digest_mismatch',409)
    body=dict(action='authorize-wallet-import',node=node,project=project,owner=owner,
              transfer=transfer,snapshot=snapshot,snapshot_digest=snapshot_digest)
    def apply(db):
        placement=a.node_project(db,node,project)
        if placement['owner']!=owner:raise Failure('legacy_owner_mismatch',409)
        old=db.execute('SELECT * FROM wallet_imports WHERE node=? AND project=?',(node,project)).fetchone()
        if old and (old['transfer'],old['digest'],old['owner'])!=(transfer,snapshot_digest,owner):
            raise Failure('wallet_import_conflict',409)
        if not old:
            db.execute("INSERT INTO wallet_imports VALUES(?,?,?,?,?,?,?,'authorized')",
                (node,project,placement['customer'],owner,transfer,encode(snapshot),snapshot_digest))
        return dict(operator_id=a.operator,node_id=node,project_id=project,customer_id=placement['customer'],
                    transfer_id=transfer,snapshot_digest=snapshot_digest,state=old['state'] if old else 'authorized',
                    amount_microcredits=snapshot['balance'])
    return a.mutation('operator',request,body,apply)


def receive(a,node,request,project,owner,transfer,snapshot_digest):
    body=dict(action='wallet-import',project=project,owner=owner,transfer=transfer,snapshot_digest=snapshot_digest)
    def apply(db):
        placement=a.node_project(db,node,project)
        row=db.execute('SELECT * FROM wallet_imports WHERE node=? AND project=?',(node,project)).fetchone()
        if not row:raise Failure('wallet_import_not_authorized',409)
        if (row['owner'],row['transfer'],row['digest'],row['customer'])!=(owner,transfer,snapshot_digest,placement['customer']):
            raise Failure('wallet_import_conflict',409)
        import json
        snapshot=json.loads(row['snapshot'])
        if row['state']=='authorized':
            customer=a.customer(db,row['customer'])
            balance=amount(customer['balance']+snapshot['balance'])
            spent=amount(customer['spent']+snapshot['spent'])
            db.execute('UPDATE customers SET balance=?,spent=? WHERE id=?',(balance,spent,row['customer']))
            db.execute("UPDATE wallet_imports SET state='credited' WHERE node=? AND project=?",(node,project))
            a.entry(db,row['customer'],project,node,'legacy_transfer',snapshot['balance'],'legacy','node:'+node,request)
        return dict(operator_id=a.operator,node_id=node,project_id=project,owner_did=owner,
                    customer_id=row['customer'],transfer_id=transfer,snapshot_digest=snapshot_digest,
                    state='credited',credited_microcredits=snapshot['balance'])
    return a.mutation('node:'+node,request,body,apply)
