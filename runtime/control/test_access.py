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

    def connect(self, node='node-one', agent=OWNER, project=PROJECT, request='connect', email='owner@example.com',trial_ip='203.0.113.10'):
        return self.access.connect(node, request, email, agent, project,trial_ip=trial_ip)

    def test_free_vm_admission_is_fleet_global_atomic_and_gateway_only(self):
        key = 'a' * 64
        first = dict(action='reserve-free-vm', project_id=PROJECT, agent_did=OWNER,
                     ssh_key_hash=key, source_ip='8.8.8.10')
        for unauthorized in ('worker1', 'admin'):
            with self.assertRaisesRegex(Failure, 'identity_gateway_credentials_required'):
                self.app.handle('POST', '/identity', unauthorized, first)
        with self.assertRaisesRegex(Failure, 'invalid_control_credentials'):
            self.app.handle('POST', '/identity', 'garbage', first)
        for ip in ('127.0.0.1', '10.0.0.1', '169.254.1.1', '::1', 'not-an-ip'):
            with self.assertRaisesRegex(Failure, 'invalid_source_ip'):
                self.app.handle('POST', '/identity', 'identity1', dict(first, source_ip=ip))
        result = self.app.handle('POST', '/identity', 'identity1', first)
        self.assertFalse(result['reused'])
        self.assertEqual(result['active_until'], 3700)
        self.assertEqual(result['claim_until'], 90100)
        self.assertTrue(self.app.handle('POST', '/identity', 'identity1', first)['reused'])
        moved = dict(first, source_ip='8.8.8.11')
        self.assertTrue(self.app.handle('POST', '/identity', 'identity1', moved)['reused'])
        self.assertTrue(self.app.handle('POST', '/identity', 'identity1',
                                        dict(first, source_ip='::ffff:8.8.8.10'))['reused'])
        second = dict(first, project_id=SECOND, agent_did=AGENT, ssh_key_hash='b' * 64)
        for ip in ('8.8.8.10', '8.8.8.11'):
            with self.assertRaisesRegex(Failure, 'free_vm_ip_already_reserved'):
                self.app.handle('POST', '/identity', 'identity2', dict(second, source_ip=ip))
        with self.assertRaisesRegex(Failure, 'free_vm_key_already_reserved'):
            self.app.handle('POST', '/identity', 'identity2', dict(second, ssh_key_hash=key, source_ip='8.8.8.12'))
        self.assertFalse(self.app.handle('POST', '/identity', 'identity2',
                                         dict(second, source_ip='8.8.8.12'))['reused'])
        # The authority's durable SQLite state is shared after restart.
        restored = Access(Authority(self.a.path, 'one', clock=lambda: 100), b'k'*32)
        self.assertTrue(restored.reserve_free_vm('node-one', PROJECT, OWNER, key, '8.8.8.10')['reused'])

    def test_free_vm_concurrent_nodes_cannot_reserve_one_ip(self):
        bodies = [dict(action='reserve-free-vm', project_id=project, agent_did=agent,
                       ssh_key_hash=letter*64, source_ip='8.8.8.20')
                  for project, agent, letter in ((PROJECT, OWNER, 'a'), (SECOND, AGENT, 'b'))]
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            futures = [pool.submit(self.app.handle, 'POST', '/identity', 'identity'+str(i+1), body)
                       for i, body in enumerate(bodies)]
            outcomes = []
            for future in futures:
                try: outcomes.append(future.result())
                except Failure as error: outcomes.append(error.code)
        self.assertEqual(sum(isinstance(value, dict) for value in outcomes), 1)
        self.assertIn('free_vm_ip_already_reserved', outcomes)

    def test_free_vm_reservation_expires_after_claim_window(self):
        first = dict(action='reserve-free-vm', project_id=PROJECT, agent_did=OWNER,
                     ssh_key_hash='a'*64, source_ip='2606:4700:4700:1::10')
        self.app.handle('POST', '/identity', 'identity1', first)
        second = dict(first, project_id=SECOND, agent_did=AGENT,
                      ssh_key_hash='b'*64, source_ip='2606:4700:4700:1::20')
        with self.assertRaisesRegex(Failure, 'free_vm_ip_already_reserved'):
            self.app.handle('POST', '/identity', 'identity2', second)
        self.a.clock = lambda: 3700
        with self.assertRaisesRegex(Failure, 'free_vm_no_longer_active'):
            self.app.handle('POST', '/identity', 'identity1', first)
        self.a.clock = lambda: 90100
        with self.assertRaisesRegex(Failure, 'free_vm_expired_pending_cleanup'):
            self.app.handle('POST', '/identity', 'identity1', first)
        with self.assertRaisesRegex(Failure, 'free_vm_ip_already_reserved'):
            self.app.handle('POST', '/identity', 'identity2', second)
        finished = dict(action='finish-free-vm', project_id=PROJECT, agent_did=OWNER)
        with self.assertRaisesRegex(Failure, 'free_vm_identity_conflict'):
            self.app.handle('POST', '/identity', 'identity2', finished)
        self.assertTrue(self.app.handle('POST', '/identity', 'identity1', finished)['released'])
        self.assertTrue(self.app.handle('POST', '/identity', 'identity1', finished)['released'])
        # The same visitor can start a fresh trial with the same IP and key
        # after the old VM and disk have actually been destroyed.
        again = dict(first, project_id=SECOND, agent_did=AGENT)
        self.assertFalse(self.app.handle('POST', '/identity', 'identity1', again)['reused'])

    def test_verified_claim_binds_existing_project_once_and_releases_ip(self):
        reservation = dict(action='reserve-free-vm', project_id=PROJECT, agent_did=OWNER,
                           ssh_key_hash='a'*64, source_ip='8.8.8.40')
        self.app.handle('POST', '/identity', 'identity1', reservation)
        claim = dict(action='claim-free-vm', request_id='claim-one', project_id=PROJECT,
                     agent_did=OWNER, email='owner@example.com')
        with self.assertRaisesRegex(Failure, 'identity_gateway_credentials_required'):
            self.app.handle('POST', '/identity', 'worker1', claim)
        result = self.app.handle('POST', '/identity', 'identity1', claim)
        self.assertEqual(result['free_vm_claimed_at'],100)
        self.assertFalse(result['trial_credit_granted'])
        self.assertEqual(result['project_id'], PROJECT)
        self.assertEqual(self.app.handle('POST', '/identity', 'identity1', claim)['customer_id'],result['customer_id'])
        with self.assertRaisesRegex(Failure, 'free_vm_already_claimed'):
            self.app.handle('POST', '/identity', 'identity1', reservation)
        with self.a.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM free_vm_trial_ips').fetchone()[0],0)
            self.assertEqual(db.execute('SELECT count(*) FROM trial_grants').fetchone()[0],0)
        other = dict(reservation, project_id=SECOND, agent_did=AGENT, ssh_key_hash='b'*64)
        self.assertFalse(self.app.handle('POST', '/identity', 'identity2', other)['reused'])

    def test_free_vm_claim_after_deadline_does_not_create_account(self):
        reservation = dict(action='reserve-free-vm', project_id=PROJECT, agent_did=OWNER,
                           ssh_key_hash='a'*64, source_ip='8.8.8.41')
        self.app.handle('POST', '/identity', 'identity1', reservation)
        self.a.clock = lambda: 90100
        with self.assertRaisesRegex(Failure, 'free_vm_claim_expired'):
            self.app.handle('POST', '/identity', 'identity1', dict(action='claim-free-vm',
                request_id='late',project_id=PROJECT,agent_did=OWNER,email='owner@example.com'))
        with self.a.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM customers').fetchone()[0],0)

    def test_same_verified_email_shares_customer_but_not_agent_grants(self):
        first=self.connect()
        second=self.connect('node-two',AGENT,SECOND,email='Owner@example.com')
        self.assertEqual(first['customer_id'],second['customer_id'])
        actor=self.a.authenticate(first['credential']['token'])
        self.assertEqual([p['id'] for p in self.a.projects(actor['customer'],actor['agent'])['projects']], [PROJECT])
        with self.assertRaisesRegex(Failure,'project_management_required'):
            self.access.issue(actor,SECOND)
        self.assertEqual(self.a.wallet(first['customer_id'])['balance_microcredits'],1_000_000)
        self.assertFalse(first['legacy_balance_transferred'])

    def test_concurrent_connection_is_atomic_and_replay_does_not_store_tokens(self):
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            results=list(pool.map(lambda _:self.connect(),range(2)))
        self.assertEqual(results[0]['customer_id'],results[1]['customer_id'])
        self.assertNotEqual(results[0]['credential']['token'],results[1]['credential']['token'])
        with self.a.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM customers').fetchone()[0],1)
            self.assertEqual(db.execute('SELECT count(*) FROM trial_grants').fetchone()[0],1)
            grant=db.execute("SELECT delta,source,actor,operation FROM wallet_entries WHERE kind='funding'").fetchone()
            self.assertEqual(tuple(grant),(1_000_000,'promotional','verified-email','microvm-trial-v1'))
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
        self.assertEqual(restored.connect('node-one','connect','owner@example.com',OWNER,PROJECT,trial_ip='203.0.113.10')['customer_id'],first['customer_id'])

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

    def test_migrated_node_capability_requires_binding_and_existing_grant(self):
        result=self.connect();token=result['credential']['token']
        body={'project_id':PROJECT,'node_id':'node-two'}
        with self.assertRaisesRegex(Failure,'project_node_mismatch'):
            self.app.handle('POST','/v1/project-token',token,body)
        with self.a.db() as db:
            db.execute('INSERT INTO vm_host_projects VALUES(?,?)',(PROJECT,'node-two'))
        grant=self.app.handle('POST','/v1/project-token',token,body)
        claims=json.loads(base64.urlsafe_b64decode(grant['token'].split('.')[1]+'=='))
        self.assertEqual(claims['node_id'],'node-two')
        self.assertEqual(claims['owner_did'],OWNER)
        self.assertEqual(self.app.handle('POST','/v1/project-token',token,{'project_id':PROJECT})['node_id'],'node-one')
        self.connect('node-two',AGENT,SECOND,request='other')
        viewer=self.a.authenticate(self.a.issue(result['customer_id'],AGENT)['token'])
        with self.assertRaisesRegex(Failure,'project_management_required'):
            self.access.issue(viewer,PROJECT,node_id='node-two')
        self.a.grant('operator','viewer',PROJECT,AGENT,'viewer')
        self.assertEqual(self.access.issue(viewer,PROJECT,read_only=True,node_id='node-two')['scope'],'project.vm.read')
        with self.assertRaisesRegex(Failure,'project_management_required'):
            self.access.issue(viewer,PROJECT,node_id='node-two')

    def test_viewer_gets_only_explicit_read_capability(self):
        first=self.connect();self.connect('node-two',AGENT,SECOND,request='second')
        self.a.grant('operator','read',PROJECT,AGENT,'viewer')
        actor=self.a.authenticate(self.a.issue(first['customer_id'],AGENT)['token'])
        with self.assertRaisesRegex(Failure,'project_management_required'):self.access.issue(actor,PROJECT)
        result=self.access.issue(actor,PROJECT,read_only=True)
        claims=json.loads(base64.urlsafe_b64decode(result['token'].split('.')[1]+'=='))
        self.assertEqual(claims['scope'],'project.vm.read')
        self.a.grant('operator','revoke',PROJECT,AGENT,'none')
        with self.assertRaises(Failure):self.access.issue(actor,PROJECT,read_only=True)

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
        linked=self.connect();human=self.access.human_login('owner@example.com','203.0.113.10')['token']
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

    def test_verified_human_receives_one_trial_credit_once(self):
        first=self.access.human_login('new@example.com','203.0.113.20')
        second=self.access.human_login('New@example.com','203.0.113.20')
        self.assertEqual(first['customer_id'],second['customer_id'])
        self.assertEqual(self.a.wallet(first['customer_id'])['total_remaining_microcredits'],1_000_000)
        with self.a.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM trial_grants').fetchone()[0],1)
            self.assertEqual(db.execute("SELECT count(*) FROM wallet_entries WHERE source='promotional'").fetchone()[0],1)
        self.assertEqual(self.app.handle('GET','/v1/projects',first['token'],{})['projects'],[])

    def test_ipv6_64_can_receive_only_one_trial(self):
        first=self.connect(trial_ip='2001:db8:1::10')
        second=self.connect('node-two',AGENT,SECOND,request='second',email='second@example.com',trial_ip='2001:db8:1::20')
        third=self.connect('node-two','did:gap:'+'c'*64,'prj_'+'c'*24,request='third',email='third@example.com',trial_ip='2001:db8:2::20')
        self.assertEqual(self.a.wallet(first['customer_id'])['total_remaining_microcredits'],1_000_000)
        self.assertEqual(self.a.wallet(second['customer_id'])['total_remaining_microcredits'],0)
        self.assertEqual(self.a.wallet(third['customer_id'])['total_remaining_microcredits'],1_000_000)
        self.assertTrue(first['trial_credit_granted'])
        self.assertFalse(second['trial_credit_granted'])
        self.assertTrue(third['trial_credit_granted'])
        with self.a.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM trial_ip_grants').fetchone()[0],2)

    def test_ipv4_can_receive_only_one_trial_and_is_stored_as_a_digest(self):
        first=self.connect(trial_ip='203.0.113.40')
        second=self.connect('node-two',AGENT,SECOND,request='second',email='second@example.com',trial_ip='203.0.113.40')
        self.assertTrue(first['trial_credit_granted'])
        self.assertFalse(second['trial_credit_granted'])
        with self.a.db() as db:
            keys=[row[0] for row in db.execute('SELECT ip_key FROM trial_ip_grants')]
        self.assertEqual(len(keys),1)
        self.assertRegex(keys[0],r'^[0-9a-f]{64}$')
        self.assertNotEqual(keys[0],'203.0.113.40')

    def test_missing_or_invalid_ip_creates_account_without_promotional_credit(self):
        result=self.connect(trial_ip='unknown')
        self.assertFalse(result['trial_credit_granted'])
        self.assertEqual(self.a.wallet(result['customer_id'])['total_remaining_microcredits'],0)
