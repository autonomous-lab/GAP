"""Operator-scoped account linking and short-lived project capabilities.

Only identity gateways assert verified email ownership. Worker credentials have
no access to this endpoint. No local wallet is imported or activated here.
"""
import base64
import hashlib
import re
import secrets

from authority import Failure, encode, identifier


class Access:
    def __init__(self, authority, seed):
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
        from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat
        self.a = authority
        self.key = Ed25519PrivateKey.from_private_bytes(seed)
        self.public_key = self.key.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw).hex()
        with authority.db() as db:
            db.execute('CREATE TABLE IF NOT EXISTS detached_members(agent TEXT PRIMARY KEY,customer TEXT NOT NULL)')
            db.execute('''CREATE TABLE IF NOT EXISTS verified_emails(email TEXT PRIMARY KEY,
                customer TEXT NOT NULL REFERENCES customers(id))''')
            db.execute('''CREATE TABLE IF NOT EXISTS identity_sources(agent TEXT PRIMARY KEY,
                node TEXT NOT NULL,email TEXT NOT NULL REFERENCES verified_emails(email))''')

    def connect(self, node, request, email, agent, project, manual_reason=None):
        # Gateway validates its durable email proof and local owner bearer.
        if not isinstance(email, str) or len(email) > 254 or not re.fullmatch(r'[^\s@<>;,"\x00-\x1f]+@[^\s@<>;,"\x00-\x1f]+', email) or not email.isascii():
            raise Failure('invalid_verified_email')
        email = email.lower()
        if not isinstance(agent, str) or not re.fullmatch(r'did:gap:[0-9a-f]{64}', agent):
            raise Failure('invalid_agent')
        if not isinstance(project, str) or not re.fullmatch(r'prj_[0-9a-f]{24}', project):
            raise Failure('invalid_project')
        identifier(node)
        body = dict(action='connect', email=email, agent=agent, project=project)
        if manual_reason is not None:
            if not isinstance(manual_reason, str) or not 10 <= len(manual_reason) <= 1000 or any(ord(c) < 32 for c in manual_reason):
                raise Failure('invalid_manual_confirmation')
            body.update(manual_reason=manual_reason, node=node)

        def apply(db):
            if db.execute('SELECT 1 FROM detached_members WHERE agent=?',(agent,)).fetchone():
                raise Failure('membership_detached',403)
            source = db.execute('SELECT * FROM identity_sources WHERE agent=?', (agent,)).fetchone()
            if source and (source['node'], source['email']) != (node, email):
                raise Failure('identity_source_conflict', 409)
            human = db.execute('SELECT customer FROM verified_emails WHERE email=?', (email,)).fetchone()
            member = db.execute("SELECT customer FROM principals WHERE kind='agent' AND subject=?", (agent,)).fetchone()
            # Existing operator bindings need explicit reconciliation, never an
            # automatic merge of two accounts or assumption about an old email.
            if member and (not human or member[0] != human[0]):
                raise Failure('legacy_identity_reconciliation_required', 409)
            customer = human[0] if human else 'cus_' + secrets.token_hex(16)
            if not human:
                db.execute('INSERT INTO customers(id,label,created) VALUES(?,?,?)', (customer, 'Verified customer', int(self.a.clock())))
                db.execute('INSERT INTO verified_emails VALUES(?,?)', (email, customer))
                subject = 'email:' + hashlib.sha256(email.encode()).hexdigest()
                db.execute("INSERT INTO principals VALUES('human',?,?,1)", (subject, customer))
            old = db.execute('SELECT * FROM projects WHERE id=?', (project,)).fetchone()
            if old and (old['customer'], old['node'], old['owner']) != (customer, node, agent):
                raise Failure('project_binding_conflict', 409)
            db.execute("INSERT OR IGNORE INTO principals VALUES('agent',?,?,0)", (agent, customer))
            db.execute('INSERT OR IGNORE INTO identity_sources VALUES(?,?,?)', (agent, node, email))
            db.execute('INSERT OR IGNORE INTO projects VALUES(?,?,?,?)', (project, customer, node, agent))
            db.execute("INSERT OR IGNORE INTO grants VALUES(?,?,'owner')", (project, agent))
            return dict(customer_id=customer, agent_did=agent, project_id=project, node_id=node,
                        operator_id=self.a.operator, legacy_balance_transferred=False,
                        confirmation_method="operator_confirmation" if manual_reason else "identity_gateway",
                        confirmation_reason=manual_reason)
        result = self.a.mutation('operator' if manual_reason else 'identity:'+node, request, body, apply)
        # Never persist a credential in an idempotency result. Repeating the
        # same successful connection mints a fresh bounded credential.
        return dict(result, **{'credential': self.a.issue(result['customer_id'], agent)})

    def reconnect(self,node,agent,project):
        with self.a.db() as db:
            source=db.execute('SELECT * FROM identity_sources WHERE agent=? AND node=?',(agent,node)).fetchone()
            p=db.execute('SELECT * FROM projects WHERE id=? AND node=? AND owner=?',(project,node,agent)).fetchone()
            member=db.execute("SELECT customer FROM principals WHERE kind='agent' AND subject=?",(agent,)).fetchone()
            detached=db.execute('SELECT 1 FROM detached_members WHERE agent=?',(agent,)).fetchone()
            if not source or not p or not member or member[0]!=p['customer'] or detached:
                raise Failure('confirmed_identity_required',403)
            customer=p['customer']
        return dict(operator_id=self.a.operator,customer_id=customer,project_id=project,node_id=node,
                    agent_did=agent,legacy_balance_transferred=False,credential=self.a.issue(customer,agent))

    def human_login(self, email):
        if not isinstance(email,str) or len(email)>254 or not re.fullmatch(r'[^\s@<>;,"\x00-\x1f]+@[^\s@<>;,"\x00-\x1f]+',email) or not email.isascii():raise Failure('invalid_verified_email')
        with self.a.db() as db:
            row=db.execute('SELECT customer FROM verified_emails WHERE email=?',(email.lower(),)).fetchone()
            if row:customer=row[0]
            else:
                customer='cus_'+secrets.token_hex(16)
                db.execute('INSERT INTO customers(id,label,created) VALUES(?,?,?)',(customer,'Verified customer',int(self.a.clock())))
                db.execute('INSERT INTO verified_emails VALUES(?,?)',(email.lower(),customer))
                db.execute("INSERT INTO principals VALUES('human',?,?,1)",('email:'+hashlib.sha256(email.lower().encode()).hexdigest(),customer))
        return self.a.issue(customer,None)

    def members(self, actor):
        if actor['agent'] is not None:raise Failure('human_account_required',403)
        with self.a.db() as db:
            rows=db.execute("SELECT subject AS agent_did FROM principals WHERE customer=? AND kind='agent' ORDER BY subject",(actor['customer'],)).fetchall()
            grants=db.execute('SELECT g.project,g.agent,g.role FROM grants g JOIN projects p ON p.id=g.project WHERE p.customer=? ORDER BY g.project,g.agent',(actor['customer'],)).fetchall()
        return dict(members=[dict(r) for r in rows],grants=[dict(r) for r in grants])

    def membership(self, actor, body):
        if actor['agent'] is not None:raise Failure('human_account_required',403)
        customer=actor['customer'];agent=body['agent_did'];request=body['request_id'];action=body['action']
        if not isinstance(agent,str) or not re.fullmatch(r'did:gap:[0-9a-f]{64}',agent):raise Failure('invalid_agent')
        if action=='issue-token':
            return self.a.issue(customer,agent)
        def apply(db):
            member=db.execute("SELECT customer FROM principals WHERE kind='agent' AND subject=?",(agent,)).fetchone()
            if action=='attach':
                if member and member[0]!=customer:raise Failure('principal_binding_conflict',409)
                db.execute('DELETE FROM detached_members WHERE agent=? AND customer=?',(agent,customer))
                if db.execute('SELECT 1 FROM detached_members WHERE agent=?',(agent,)).fetchone():raise Failure('principal_binding_conflict',409)
                db.execute("INSERT OR IGNORE INTO principals VALUES('agent',?,?,0)",(agent,customer))
            else:
                if not member or member[0]!=customer:raise Failure('agent_membership_required',403)
                if action=='detach':
                    if db.execute('SELECT 1 FROM projects WHERE owner=?',(agent,)).fetchone():raise Failure('project_owner_cannot_be_detached',409)
                    db.execute('INSERT OR REPLACE INTO detached_members VALUES(?,?)',(agent,customer))
                    db.execute('DELETE FROM grants WHERE agent=?',(agent,))
                    db.execute('UPDATE credentials SET revoked=1 WHERE customer=? AND agent=?',(customer,agent))
                    db.execute("DELETE FROM principals WHERE kind='agent' AND subject=?",(agent,))
                elif action=='grant':
                    role=body['role'];project=body['project_id']
                    if role not in ('viewer','operator','none'):raise Failure('invalid_project_role')
                    p=db.execute('SELECT * FROM projects WHERE id=? AND customer=?',(project,customer)).fetchone()
                    if not p:raise Failure('project_membership_required',403)
                    if p['owner']==agent:raise Failure('owner_grant_immutable',409)
                    db.execute('DELETE FROM grants WHERE project=? AND agent=?',(project,agent))
                    if role!='none':db.execute('INSERT INTO grants VALUES(?,?,?)',(project,agent,role))
                else:raise Failure('unknown_membership_action')
            return dict(updated=True,action=action,agent_did=agent)
        return self.a.mutation('human:'+customer,request,body,apply)

    def issue(self, actor, project, ttl=120, read_only=False):
        if type(read_only) is not bool:raise Failure('invalid_read_only')
        if type(ttl) is not int or not 30 <= ttl <= 300:
            raise Failure('invalid_token_lifetime')
        with self.a.db() as db:
            row = db.execute('SELECT * FROM projects WHERE id=? AND customer=?', (project, actor['customer'])).fetchone()
            if not row:
                raise Failure('project_membership_required', 403)
            if actor['agent'] is not None:
                grant = db.execute('SELECT role FROM grants WHERE project=? AND agent=?', (project, actor['agent'])).fetchone()
                if not grant or grant[0] not in (('owner','operator','viewer') if read_only else ('owner','operator')):
                    raise Failure('project_management_required', 403)
            now = int(self.a.clock())
            claims = dict(version=1, operator_id=self.a.operator, node_id=row['node'],
                          customer_id=row['customer'], project_id=project, owner_did=row['owner'],
                          agent_did=actor['agent'], scope='project.vm.read' if read_only else 'project.manage', issued_at=now,
                          expires_at=now+ttl, nonce=secrets.token_hex(16))
        payload = base64.urlsafe_b64encode(encode(claims).encode()).rstrip(b'=')
        message = b'gapf1.' + payload
        signature = base64.urlsafe_b64encode(self.key.sign(message)).rstrip(b'=')
        return dict(token=(message+b'.'+signature).decode(), expires_at=now+ttl,
                    operator_id=self.a.operator, node_id=row['node'], project_id=project, scope=claims['scope'])
