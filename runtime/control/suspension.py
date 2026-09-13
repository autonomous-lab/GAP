"""Operator decisions shared by every execution node, independent of local bans."""
import hashlib
from authority import Failure


def schema(db):
    db.execute('''CREATE TABLE IF NOT EXISTS customer_suspensions(
        subject TEXT PRIMARY KEY, active INTEGER NOT NULL, revision INTEGER NOT NULL,
        reason TEXT NOT NULL, hold_until INTEGER, updated INTEGER NOT NULL)''')
    db.execute('''CREATE TABLE IF NOT EXISTS suspension_events(
        sequence INTEGER PRIMARY KEY AUTOINCREMENT, subject TEXT NOT NULL,
        active INTEGER NOT NULL, revision INTEGER NOT NULL, reason TEXT NOT NULL,
        hold_until INTEGER, updated INTEGER NOT NULL)''')


def check(db, customer):
    if db.execute('SELECT 1 FROM customer_suspensions WHERE subject IN (?,?) AND active=1',
                  (customer, '*')).fetchone():
        raise Failure('customer_suspended', 403)


def hold(db, customer):
    return db.execute('SELECT max(hold_until) FROM customer_suspensions WHERE subject IN (?,?) AND active=1',
                      (customer, '*')).fetchone()[0]


def set_policy(a, request, body):
    subject = body.get('customer_id') if body.get('scope', 'customer') == 'customer' else '*'
    if body.get('scope', 'customer') not in ('customer', 'operator'):
        raise Failure('invalid_suspension_scope')
    active = body.get('active'); revision = body.get('expected_revision'); reason = body.get('reason')
    hours = body.get('retention_hours', 72)
    if (type(active) is not bool or type(revision) is not int or revision < 0
            or not isinstance(reason, str) or not 10 <= len(reason) <= 1000
            or any(ord(c) < 32 for c in reason) or type(hours) is not int or not 0 <= hours <= 720):
        raise Failure('invalid_suspension_decision')
    def apply(db):
        if subject != '*': a.customer(db, subject)
        old = db.execute('SELECT revision FROM customer_suspensions WHERE subject=?', (subject,)).fetchone()
        if (old[0] if old else 0) != revision: raise Failure('suspension_revision_conflict', 409)
        if active and hours and db.execute("SELECT 1 FROM retention_claims r JOIN projects p ON p.id=r.project WHERE r.state='claimed' AND (?='*' OR p.customer=?)",(subject,subject)).fetchone():
            raise Failure('retention_already_in_progress',409)
        now = int(a.clock()); until = now + hours * 3600 if active and hours else None
        values = (subject, int(active), revision + 1, reason, until, now)
        db.execute('INSERT OR REPLACE INTO customer_suspensions VALUES(?,?,?,?,?,?)', values)
        db.execute('INSERT INTO suspension_events(subject,active,revision,reason,hold_until,updated) VALUES(?,?,?,?,?,?)', values)
        if active:
            if subject == '*': db.execute('UPDATE credentials SET revoked=1')
            else: db.execute('UPDATE credentials SET revoked=1 WHERE customer=?', (subject,))
        return dict(subject=subject, active=active, revision=revision+1, reason=reason, hold_until=until,
                    updated=now, restoration='explicit_operator_action')
    return a.mutation('operator', request, dict(body, action='suspension-set'), apply)


def status(a, subject):
    with a.db() as db:
        if subject != '*': a.customer(db, subject)
        row = db.execute('SELECT * FROM customer_suspensions WHERE subject=?', (subject,)).fetchone()
        events = db.execute('SELECT * FROM suspension_events WHERE subject=? ORDER BY sequence DESC LIMIT 100', (subject,)).fetchall()
        return dict(policy=dict(row) if row else dict(subject=subject, active=False, revision=0),
                    history=[dict(r) for r in events])


def snapshot(a, node):
    with a.db() as db:
        sequence = db.execute('SELECT coalesce(max(sequence),0) FROM suspension_events').fetchone()[0]
        all_blocked = db.execute("SELECT 1 FROM customer_suspensions WHERE subject='*' AND active=1").fetchone() is not None
        customers = [r[0] for r in db.execute("SELECT subject FROM customer_suspensions WHERE active=1 AND subject!='*'")]
        agents = []; emails = []; projects = []
        for customer in customers:
            agents.extend(r[0] for r in db.execute("SELECT subject FROM principals WHERE customer=? AND kind='agent'", (customer,)))
            projects.extend(r[0] for r in db.execute('SELECT id FROM projects WHERE customer=?', (customer,)))
            # Nodes compare verified local addresses without receiving raw email addresses.
            if db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='verified_emails'").fetchone():
                emails.extend(hashlib.sha256(r[0].lower().encode()).hexdigest() for r in db.execute('SELECT email FROM verified_emails WHERE customer=?', (customer,)))
        result = dict(protocol=1, operator_id=a.operator, node_id=node, sequence=sequence,
                      all_blocked=all_blocked, agents=sorted(set(agents)), email_hashes=sorted(set(emails)),
                      projects=sorted(set(projects)))
        from authority import encode
        if len(encode(result).encode()) > 60000: raise Failure('suspension_snapshot_too_large', 503)
        return result
