"""Central 72-hour retention; a partition or held allowance is never exhaustion."""
from authority import Failure, identifier

SECONDS = 72 * 3600


def schema(db):
    db.execute('CREATE TABLE IF NOT EXISTS retention_clocks(customer TEXT PRIMARY KEY, exhausted INTEGER)')
    db.execute('''CREATE TABLE IF NOT EXISTS retention_claims(node TEXT NOT NULL, project TEXT NOT NULL,
        claim TEXT NOT NULL, state TEXT NOT NULL, PRIMARY KEY(node,project))''')


def refresh(a, db, customer):
    balance=a.customer(db,customer)['balance']
    held=db.execute('SELECT coalesce(sum(allocated-consumed),0) FROM reservations WHERE customer=? AND closed=0',(customer,)).fetchone()[0]
    row=db.execute('SELECT exhausted FROM retention_clocks WHERE customer=?',(customer,)).fetchone()
    since=None if balance+held else row[0] if row and row[0] is not None else int(a.clock())
    db.execute('INSERT INTO retention_clocks VALUES(?,?) ON CONFLICT(customer) DO UPDATE SET exhausted=excluded.exhausted',(customer,since))
    return since


def status(a,node,project,owner):
    with a.db() as db:
        p=a.node_project(db,node,project)
        if p['owner']!=owner:raise Failure('retention_owner_mismatch',403)
        since=refresh(a,db,p['customer'])
        claim=db.execute('SELECT claim,state FROM retention_claims WHERE node=? AND project=?',(node,project)).fetchone()
        return dict(exhausted_at=since,delete_after=since+SECONDS if since is not None else None,
                    claim_id=claim['claim'] if claim and claim['state']=='claimed' else None)


def claim(a,node,project,owner,claim_id):
    identifier(claim_id)
    with a.db() as db:
        p=a.node_project(db,node,project)
        if p['owner']!=owner:raise Failure('retention_owner_mismatch',403)
        old=db.execute('SELECT * FROM retention_claims WHERE node=? AND project=?',(node,project)).fetchone()
        if old and old['claim']==claim_id:return dict(claim_id=claim_id,state=old['state'])
        if old and old['state']=='claimed':raise Failure('retention_claim_conflict',409)
        since=refresh(a,db,p['customer'])
        if since is None or a.clock()<since+SECONDS:raise Failure('retention_not_due',409)
        db.execute("INSERT INTO retention_claims VALUES(?,?,?,'claimed') ON CONFLICT(node,project) DO UPDATE SET claim=excluded.claim,state='claimed'",(node,project,claim_id))
        # The claim fences this project's future reservations even if the owner
        # tops up immediately afterwards. Other projects can use the new funds.
        return dict(claim_id=claim_id,state='claimed')


def finish(a,node,project,owner,claim_id):
    with a.db() as db:
        p=a.node_project(db,node,project)
        if p['owner']!=owner:raise Failure('retention_owner_mismatch',403)
        old=db.execute('SELECT * FROM retention_claims WHERE node=? AND project=?',(node,project)).fetchone()
        if not old or old['claim']!=claim_id:raise Failure('retention_claim_conflict',409)
        db.execute("UPDATE retention_claims SET state='finished' WHERE node=? AND project=?",(node,project))
        return dict(claim_id=claim_id,state='finished')
