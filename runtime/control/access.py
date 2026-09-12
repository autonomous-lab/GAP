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
            db.execute('''CREATE TABLE IF NOT EXISTS verified_emails(email TEXT PRIMARY KEY,
                customer TEXT NOT NULL REFERENCES customers(id))''')
            db.execute('''CREATE TABLE IF NOT EXISTS identity_sources(agent TEXT PRIMARY KEY,
                node TEXT NOT NULL,email TEXT NOT NULL REFERENCES verified_emails(email))''')

    def connect(self, node, request, email, agent, project):
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
        def apply(db):
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
                        operator_id=self.a.operator, legacy_balance_transferred=False)
        result = self.a.mutation('identity:'+node, request, body, apply)
        # Never persist a credential in an idempotency result. Repeating the
        # same successful connection mints a fresh bounded credential.
        return dict(result, **{'credential': self.a.issue(result['customer_id'], agent)})

    def issue(self, actor, project, ttl=120):
        if type(ttl) is not int or not 30 <= ttl <= 300:
            raise Failure('invalid_token_lifetime')
        with self.a.db() as db:
            row = db.execute('SELECT * FROM projects WHERE id=? AND customer=?', (project, actor['customer'])).fetchone()
            if not row:
                raise Failure('project_membership_required', 403)
            if actor['agent'] is not None:
                grant = db.execute('SELECT role FROM grants WHERE project=? AND agent=?', (project, actor['agent'])).fetchone()
                if not grant or grant[0] not in ('owner', 'operator'):
                    raise Failure('project_management_required', 403)
            now = int(self.a.clock())
            claims = dict(version=1, operator_id=self.a.operator, node_id=row['node'],
                          customer_id=row['customer'], project_id=project, owner_did=row['owner'],
                          agent_did=actor['agent'], scope='project.manage', issued_at=now,
                          expires_at=now+ttl, nonce=secrets.token_hex(16))
        payload = base64.urlsafe_b64encode(encode(claims).encode()).rstrip(b'=')
        message = b'gapf1.' + payload
        signature = base64.urlsafe_b64encode(self.key.sign(message)).rstrip(b'=')
        return dict(token=(message+b'.'+signature).decode(), expires_at=now+ttl,
                    operator_id=self.a.operator, node_id=row['node'], project_id=project, scope='project.manage')
