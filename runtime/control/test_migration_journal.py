"""Migration preparation recovery and authorization; no hypervisor needed."""
import concurrent.futures
import unittest
from authority import Authority, Failure
from service import Application
import migration_journal as journal
import test_authority
from test_authority import OWNER, PROJECT
VM = 'vm_' + 'a' * 32
DIGEST = 'b' * 64

class MigrationJournalTests(unittest.TestCase):
    def setUp(self):
        test_authority.AuthorityTests.setUp(self)
        p = self.a.capacity_prepare('node-one', 'allocate', PROJECT, OWNER, VM, 4, 1024, 0)
        self.allocation = self.a.capacity_finish('node-one', 'allocated', PROJECT, VM,
            p['revision'], p['transition_id'], 'commit', 'created-locally')
        self.app = Application(self.a, 'admin', {'node-one':'one','node-two':'two','node-three':'three'},
            allow_capacity=True, allow_reservations=True, allow_migration_journal=True)
    def prepare(self, request='move'):
        return self.app.handle('POST', '/operator', 'admin', dict(action='migration-prepare',
            request_id=request,customer_id=self.customer,project_id=PROJECT,vm_id=VM,
            source_node='node-one',target_node='node-two',capacity_revision=self.allocation['revision']))
    def attest(self, row, node='one', stage='source-fenced', request='source', digest=DIGEST):
        return self.app.handle('POST','/node',node,dict(action='migration-attest',request_id=request,
            migration_id=row['migration_id'],revision=row['revision'],stage=stage,
            evidence_id='durable-'+request,disk_sha256=digest))
    def fails(self, code, fn):
        test_authority.AuthorityTests.fails(self,code,fn)
    def test_restart_replay_never_grants_execution_or_changes_money(self):
        wallet = self.a.wallet(self.customer)
        quota = self.a.quotas(self.customer)
        first = self.prepare()
        source = self.attest(first)
        self.a = Authority(self.path,'operator-one'); self.app.authority = self.a
        self.assertEqual(self.prepare(),first)
        self.assertEqual(self.attest(first),source)
        self.assertEqual(journal.get(self.a,first['migration_id'])['phase'],'source_fenced')
        target = self.attest(source,'two','target-staged','target')
        self.assertEqual(target['phase'],'target_staged')
        self.assertFalse(target['execution_authorized'])
        self.assertFalse(target['handoff_complete'])
        self.assertEqual(self.a.wallet(self.customer),wallet)
        self.assertEqual(self.a.quotas(self.customer),quota)
        self.assertEqual(self.a.capacity_get('node-one',PROJECT,VM)['node_id'],'node-one')
        self.fails('project_node_mismatch',lambda:self.a.capacity_get('node-two',PROJECT,VM))
    def test_wrong_worker_order_revision_and_digest_are_rejected(self):
        first = self.prepare()
        self.fails('migration_node_mismatch',lambda:self.attest(first,'two'))
        self.fails('migration_phase_conflict',lambda:self.attest(first,'two','target-staged','early'))
        source = self.attest(first)
        self.fails('migration_revision_conflict',lambda:self.attest(first,request='stale'))
        self.fails('migration_disk_digest_mismatch',lambda:self.attest(source,'two','target-staged','bad','c'*64))
        self.fails('migration_node_mismatch',lambda:journal.get(self.a,first['migration_id'],'node-three'))
        self.assertEqual(journal.get(self.a,first['migration_id'])['revision'],2)
    def test_concurrent_prepares_lock_capacity(self):
        def attempt(request):
            a=Authority(self.path,'operator-one')
            try:return journal.prepare(a,request,self.customer,PROJECT,VM,'node-one','node-two',self.allocation['revision'])['phase']
            except Failure as e:return e.code
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            self.assertCountEqual(list(pool.map(attempt,['move-one','move-two'])),['prepared','vm_migration_in_progress'])
        self.fails('vm_migration_in_progress',lambda:self.a.capacity_prepare('node-one','resize',PROJECT,OWNER,VM,2,512,self.allocation['revision']))
        self.fails('vm_migration_in_progress',lambda:self.a.capacity_finish('node-one','release',PROJECT,VM,self.allocation['revision'],None,'release','destroyed'))
    def test_placements_filter_memberships_and_keep_source_until_handoff(self):
        from test_authority import AGENT
        self.a.attach_principal('operator','member',self.customer,'agent',AGENT)
        self.assertEqual(self.a.vm_placements(self.customer,AGENT)['placements'],[])
        self.a.grant('operator','view',PROJECT,AGENT,'viewer')
        first=self.prepare();source=self.attest(first)
        self.attest(source,'two','target-staged','target')
        token=self.a.issue(self.customer,AGENT)['token']
        result=self.app.handle('GET','/v1/vm-placements',token,{})['placements']
        self.assertEqual(len(result),1)
        self.assertEqual(result[0]['node_id'],'node-one')
        self.assertEqual(result[0]['migration']['target_node'],'node-two')
        self.assertFalse(result[0]['migration']['handoff_complete'])
        self.a.grant('operator','revoke',PROJECT,AGENT,'none')
        self.assertEqual(self.a.vm_placements(self.customer,AGENT)['placements'],[])
        other=self.a.create_customer('operator','other-account','Other')['customer_id']
        self.assertEqual(self.a.vm_placements(other)['placements'],[])

    def test_cancel_requires_target_discard_before_source_restore(self):
        first=self.prepare();source=self.attest(first)
        cancelling=journal.cancel_request(self.a,'cancel',first['migration_id'],source['revision'])
        self.fails('migration_phase_conflict',lambda:journal.cancel_attest(self.a,'node-one','early',first['migration_id'],cancelling['revision'],'source-restored','receipt'))
        self.fails('migration_revision_conflict',lambda:self.attest(source,'two','target-staged','late'))
        discarded=journal.cancel_attest(self.a,'node-two','discard',first['migration_id'],cancelling['revision'],'target-discarded','no-target-copy')
        self.fails('vm_migration_in_progress',lambda:self.prepare('too-early'))
        self.a=Authority(self.path,'operator-one');self.app.authority=self.a
        done=journal.cancel_attest(self.a,'node-one','restore',first['migration_id'],discarded['revision'],'source-restored','source-restored')
        self.assertEqual(done['phase'],'cancelled')
        self.assertFalse(done['execution_authorized'])
        self.assertEqual(journal.cancel_request(self.a,'cancel',first['migration_id'],source['revision']),cancelling)
        new=self.prepare('new-move')
        self.assertNotEqual(new['migration_id'],first['migration_id'])
        self.assertEqual(len(self.a.vm_placements(self.customer)['placements']),1)
        self.fails('migration_revision_conflict',lambda:self.attest(first,request='old-transfer'))

    def test_handoff_preserves_project_wallet_and_count_and_fences_old_node(self):
        self.a.topup('operator','fund',self.customer,100,'promotional')
        self.a.checkpoint('node-one','reserve',PROJECT,OWNER,'source-reservation',0,0,60,30)
        first=self.prepare();source=self.attest(first)
        target=self.attest(source,'two','target-staged','target')
        self.fails('migration_phase_conflict',lambda:journal.commit(self.a,'premature',first['migration_id'],target['revision']))
        self.fails('migration_checkpoint_missing',lambda:journal.settle(self.a,'node-one','settle-missing',first['migration_id'],target['revision'],'missing'))
        self.a.checkpoint('node-one','final-sample',PROJECT,OWNER,'source-reservation',10,0,50,30)
        settled=journal.settle(self.a,'node-one','settle',first['migration_id'],target['revision'],'final-sample')
        routed=journal.routes_ready(self.a,'routes',first['migration_id'],settled['revision'],'dormant-routes-installed')
        wallet=self.a.wallet(self.customer);quota=self.a.quotas(self.customer)
        committed=journal.commit(self.a,'commit',first['migration_id'],routed['revision'])
        self.assertTrue(committed['handoff_complete'])
        self.assertFalse(committed['execution_authorized'])
        self.assertEqual(self.a.wallet(self.customer),wallet)
        self.assertEqual(self.a.quotas(self.customer),quota)
        self.assertEqual(self.a.capacity_get('node-two',PROJECT,VM)['node_id'],'node-two')
        with self.a.db() as db:
            self.assertEqual(self.a.node_project(db,'node-two',PROJECT)['node'],'node-two')
        self.fails('capacity_binding_mismatch',lambda:self.a.capacity_get('node-one',PROJECT,VM))
        self.assertEqual(next(p for p in self.a.projects(self.customer)['projects'] if p['id']==PROJECT)['node'],'node-one')
        self.a=Authority(self.path,'operator-one')
        self.assertEqual(journal.commit(self.a,'commit',first['migration_id'],routed['revision']),committed)
        self.fails('migration_phase_conflict',lambda:journal.cancel_request(self.a,'undo',first['migration_id'],committed['revision']))
        self.a.checkpoint('node-two','target-reserve',PROJECT,OWNER,'target-reservation',0,0,20,30)
        self.assertEqual(self.a.wallet(self.customer)['total_remaining_microcredits'],90)
        current=self.a.capacity_get('node-two',PROJECT,VM)
        reverse=journal.prepare(self.a,'reverse',self.customer,PROJECT,VM,'node-two','node-one',current['revision'])
        self.assertNotEqual(reverse['migration_id'],first['migration_id'])
        self.assertEqual(len(self.a.vm_placements(self.customer)['placements']),1)

    def test_handoff_refuses_retention_claim_and_rolls_back_all_changes(self):
        self.a.topup('operator','fund',self.customer,100,'promotional')
        self.a.checkpoint('node-one','checkpoint',PROJECT,OWNER,'reservation',0,0,10,30)
        first=self.prepare();source=self.attest(first)
        target=self.attest(source,'two','target-staged','target')
        self.fails('migration_node_mismatch',lambda:journal.settle(self.a,'node-two','wrong-settle',first['migration_id'],target['revision'],'checkpoint'))
        settled=journal.settle(self.a,'node-one','settle',first['migration_id'],target['revision'],'checkpoint')
        ready=journal.routes_ready(self.a,'routes',first['migration_id'],settled['revision'],'routes')
        with self.a.db() as db:
            db.execute("INSERT INTO retention_claims VALUES(?,?,?,'claimed')",('node-one',PROJECT,'claim'))
        self.fails('migration_retention_claimed',lambda:journal.commit(self.a,'commit',first['migration_id'],ready['revision']))
        self.assertEqual(self.a.capacity_get('node-one',PROJECT,VM)['node_id'],'node-one')
        self.assertEqual(journal.get(self.a,first['migration_id'])['phase'],'routing_ready')
        self.fails('project_node_mismatch',lambda:self.a.capacity_get('node-two',PROJECT,VM))

    def test_default_disabled_and_operator_only(self):
        self.app.allow_migration_journal=False
        self.fails('migration_journal_disabled',self.prepare)
        self.app.allow_migration_journal=True
        self.fails('operator_credentials_required',lambda:self.app.handle('POST','/operator','one',{'action':'migration-prepare'}))
        other=self.a.create_customer('operator','other','Other')['customer_id']
        self.fails('migration_customer_mismatch',lambda:journal.prepare(self.a,'wrong-owner',other,PROJECT,VM,'node-one','node-two',self.allocation['revision']))
if __name__=='__main__':unittest.main()
