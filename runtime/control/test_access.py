import base64
import concurrent.futures
import json
from pathlib import Path
import tempfile
import unittest

from access import Access
from authority import Authority, Failure
from service import Application
from test_authority import OWNER, AGENT, PROJECT, SECOND


class AccessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.a = Authority(Path(self.temp.name)/'authority.sqlite', 'one', clock=lambda: 100)
        self.access = Access(self.a, b'k'*32)
        self.app = Application(self.a, 'admin', {'node-one':'worker1','node-two':'worker2'},
                               access=self.access, identity_nodes={'node-one':'identity1','node-two':'identity2'})

    def connect(self, node='node-one', agent=OWNER, project=PROJECT, request='connect', email='owner@example.com'):
        return self.access.connect(node, request, email, agent, project)

    def test_same_verified_email_shares_customer_but_not_agent_grants(self):
        first=self.connect()
        second=self.connect('node-two',AGENT,SECOND,email='Owner@example.com')
        self.assertEqual(first['customer_id'],second['customer_id'])
        actor=self.a.authenticate(first['credential']['token'])
        self.assertEqual([p['id'] for p in self.a.projects(actor['customer'],actor['agent'])['projects']], [PROJECT])
        with self.assertRaisesRegex(Failure,'project_management_required'):
            self.access.issue(actor,SECOND)
        self.assertEqual(self.a.wallet(first['customer_id'])['balance_microcredits'],0)
        self.assertFalse(first['legacy_balance_transferred'])

    def test_concurrent_connection_is_atomic_and_replay_does_not_store_tokens(self):
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            results=list(pool.map(lambda _:self.connect(),range(2)))
        self.assertEqual(results[0]['customer_id'],results[1]['customer_id'])
        self.assertNotEqual(results[0]['credential']['token'],results[1]['credential']['token'])
        with self.a.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM customers').fetchone()[0],1)
            self.assertNotIn('gapc_', db.execute('SELECT result FROM operations').fetchone()[0])
        with self.assertRaisesRegex(Failure,'request_id_conflict'):
            self.connect(email='other@example.com')

    def test_source_pin_conflicts_and_restart(self):
        first=self.connect()
        with self.assertRaisesRegex(Failure,'identity_source_conflict'):
            self.connect('node-two', request='other')
        with self.assertRaisesRegex(Failure,'identity_source_conflict'):
            self.connect(request='other',email='other@example.com')
        restored=Access(Authority(self.a.path,'one',clock=lambda:100),b'k'*32)
        self.assertEqual(restored.connect('node-one','connect','owner@example.com',OWNER,PROJECT)['customer_id'],first['customer_id'])

    def test_existing_operator_binding_is_not_silently_merged(self):
        customer=self.a.create_customer('operator','old','Old')['customer_id']
        self.a.attach_principal('operator','owner',customer,'agent',OWNER)
        with self.assertRaisesRegex(Failure,'legacy_identity_reconciliation_required'):
            self.connect()
        with self.a.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM verified_emails').fetchone()[0],0)

    def test_project_conflict_rolls_back_new_customer(self):
        self.connect()
        with self.assertRaisesRegex(Failure,'project_binding_conflict'):
            self.connect('node-two',AGENT,PROJECT,email='other@example.com')
        with self.a.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM customers').fetchone()[0],1)

    def test_signed_capability_has_exact_audience_and_no_email(self):
        result=self.connect()
        token=result['credential']['token']
        grant=self.app.handle('POST','/v1/project-token',token,{'project_id':PROJECT})
        prefix,payload,signature=grant['token'].split('.')
        self.access.key.public_key().verify(base64.urlsafe_b64decode(signature+'=='),(prefix+'.'+payload).encode())
        claims=json.loads(base64.urlsafe_b64decode(payload+'=='))
        self.assertEqual((claims['operator_id'],claims['node_id'],claims['owner_did']),('one','node-one',OWNER))
        self.assertEqual(claims['expires_at']-claims['issued_at'],120)
        self.assertNotIn('email',claims)
        self.a.revoke(token)
        with self.assertRaisesRegex(Failure,'invalid_control_credentials'):
            self.app.handle('POST','/v1/project-token',token,{'project_id':PROJECT})

    def test_worker_client_and_operator_cannot_assert_identity(self):
        body=dict(action='connect',request_id='connect',email='owner@example.com',agent_did=OWNER,project_id=PROJECT)
        for token in ['worker1','worker2','admin','garbage']:
            with self.assertRaises(Failure):self.app.handle('POST','/identity',token,body)
        result=self.app.handle('POST','/identity','identity1',body)
        with self.assertRaises(Failure):self.app.handle('POST','/identity',result['credential']['token'],body)
        for path in ['/operator','/node','/v1/account']:
            with self.assertRaises(Failure):self.app.handle('POST',path,'identity1',{})
        with self.assertRaises(ValueError):Application(self.a,'admin',{'node-one':'worker'},identity_nodes={'node-one':'worker'})

    def test_viewer_rejected_operator_allowed_and_revocation_stops_issuance(self):
        result=self.connect()
        customer=result['customer_id']
        self.a.attach_principal('operator','agent',customer,'agent',AGENT)
        actor={'customer':customer,'agent':AGENT}
        self.a.grant('operator','view',PROJECT,AGENT,'viewer')
        with self.assertRaisesRegex(Failure,'project_management_required'):self.access.issue(actor,PROJECT)
        self.a.grant('operator','manage',PROJECT,AGENT,'operator')
        self.assertTrue(self.access.issue(actor,PROJECT)['token'].startswith('gapf1.'))
        self.a.grant('operator','revoke',PROJECT,AGENT,'none')
        with self.assertRaises(Failure):self.access.issue(actor,PROJECT)
        for ttl in [0,29,301,True,'120']:
            with self.assertRaises(Failure):self.access.issue(actor,PROJECT,ttl)


    def test_manual_confirmation_is_operator_only_audited_and_idempotent(self):
        body=dict(action='confirm-identity',request_id='manual-confirm',node_id='node-one',
                  email='owner@example.com',agent_did=OWNER,project_id=PROJECT,
                  reason='Owner confirmed address directly in authenticated support conversation')
        for token in ['worker1','identity1','garbage']:
            with self.assertRaises(Failure):self.app.handle('POST','/operator',token,body)
        for reason in [None, '', 'short']:
            with self.assertRaises(Failure):self.app.handle('POST','/operator','admin',dict(body,reason=reason))
        first=self.app.handle('POST','/operator','admin',body)
        second=self.app.handle('POST','/operator','admin',body)
        self.assertEqual(first['customer_id'],second['customer_id'])
        self.assertEqual(first['confirmation_method'],'operator_confirmation')
        self.assertEqual(self.access.reconnect('node-one',OWNER,PROJECT)['customer_id'],first['customer_id'])
        with self.assertRaises(Failure):self.access.reconnect('node-two',OWNER,PROJECT)
        with self.a.db() as db:
            row=db.execute("SELECT result FROM operations WHERE actor='operator' AND id='manual-confirm'").fetchone()
            self.assertIn(body['reason'],row[0])
            self.assertNotIn('gapc_',row[0])
        with self.assertRaisesRegex(Failure,'request_id_conflict'):
            self.app.handle('POST','/operator','admin',dict(body,reason='Different confirmation reason'))
        with self.assertRaisesRegex(Failure,'identity_source_conflict'):
            self.app.handle('POST','/operator','admin',dict(body,request_id='other',email='another@example.com'))

    def test_human_membership_cycle_is_scoped_and_detachment_revokes(self):
        linked=self.connect();human=self.access.human_login('owner@example.com')['token']
        actor=self.a.authenticate(human)
        self.assertIsNone(actor['agent'])
        body=dict(request_id='attach',action='attach',agent_did=AGENT)
        with self.assertRaises(Failure):self.app.handle('POST','/v1/members',linked['credential']['token'],body)
        self.app.handle('POST','/v1/members',human,body)
        token=self.app.handle('POST','/v1/members',human,dict(body,action='issue-token'))['token']
        self.app.handle('POST','/v1/members',human,dict(body,request_id='grant',action='grant',project_id=PROJECT,role='operator'))
        self.assertTrue(self.access.issue(self.a.authenticate(token),PROJECT)['token'])
        self.app.handle('POST','/v1/members',human,dict(body,request_id='detach',action='detach'))
        with self.assertRaises(Failure):self.a.authenticate(token)
        with self.assertRaisesRegex(Failure,'membership_detached'):
            self.connect(agent=AGENT,project=SECOND,request='return')
        with self.assertRaisesRegex(Failure,'project_owner_cannot_be_detached'):
            self.app.handle('POST','/v1/members',human,dict(body,agent_did=OWNER,request_id='detach-owner',action='detach'))
        self.app.handle('POST','/v1/members',human,dict(body,request_id='reattach'))
        self.assertTrue(self.a.issue(actor['customer'],AGENT)['token'])

    def test_verified_human_can_open_empty_account_without_free_money(self):
        first=self.access.human_login('new@example.com')
        second=self.access.human_login('New@example.com')
        self.assertEqual(first['customer_id'],second['customer_id'])
        self.assertEqual(self.a.wallet(first['customer_id'])['total_remaining_microcredits'],0)
        self.assertEqual(self.app.handle('GET','/v1/projects',first['token'],{})['projects'],[])
