import base64
from pathlib import Path
from types import SimpleNamespace
import unittest
from access import Access
from authority import Failure
from migration_service import Migrations
import migration_journal as journal
import test_migration_journal as fixture
from test_authority import PROJECT,OWNER
VM=fixture.VM;DIGEST=fixture.DIGEST

class MigrationServiceTests(unittest.TestCase):
    def setUp(self):
        fixture.MigrationJournalTests.setUp(self)
        path=Path(self.path).parent/'peer.token';path.write_text('a'*43)
        self.tariff=dict(version='test',vcpu_hour=1,gib_ram_hour=1,gb_disk_month=1,gb_in=1,gb_out=1)
        prices=SimpleNamespace(get=lambda:{n:dict(available=True,tariff=self.tariff) for n in ('node-one','node-two')})
        self.moves=Migrations(self.a,Access(self.a,b'k'*32),{n:dict(origin='https://'+n+'.test',token_file=str(path)) for n in ('node-one','node-two')},prices)
        self.moves.launch=lambda identity:None
        self.moves.vm_api=self.vm_api;self.moves.worker=self.worker
        self.actor=dict(customer=self.customer,agent=None)
        self.body=dict(project_id=PROJECT,vm_id=VM,target_node='node-two',request_id='start',confirm_downtime=True,target_tariff=self.tariff)
        self.events=[];self.offset=0;self.interrupt=False
        self.a.topup('operator','fund-move',self.customer,100,'promotional')
        self.moves.verify_route=lambda *args:None
    def vm_api(self,row,node,suffix,method='GET',body=None):
        self.events.append(('api',suffix));return {}
    def worker(self,node,identity,operation,**body):
        self.events.append((node,operation));row=journal.get(self.a,identity)
        if operation=='node-admission':return dict(kernel_sha256=DIGEST,cpu_flags=['fpu'],free_bytes=100*1024**3,memory_available_mib=4096,vm=dict(state='running',memory_mib=1024,disk_gib=8) if node=='node-one' else None)
        if operation=='export':
            if row['phase']=='prepared':journal.attest(self.a,node,identity+':export',identity,row['revision'],'source-fenced','exported',DIGEST)
            if self.interrupt:raise Failure('migration_node_unavailable',503)
            return dict(phase='exported',size=3,archive_sha256=DIGEST)
        if operation=='upload-status':return dict(received=self.offset,archive_sha256=DIGEST,size=3)
        if operation=='read':return dict(offset=0,size=3,data_base64=base64.b64encode(b'abc').decode())
        if operation=='write':self.offset=3;return dict(received=3)
        if operation=='import':
            journal.attest(self.a,node,identity+':import',identity,row['revision'],'target-staged','imported',DIGEST)
            return dict(phase='imported')
        if operation=='settle':
            self.a.checkpoint(node,'final-checkpoint',PROJECT,OWNER,'source-reservation',0,0,60,30)
            return journal.settle(self.a,node,identity+':settle',identity,row['revision'],'final-checkpoint')
        if operation=='route':return dict(evidence_id=identity+':routes')
        if operation=='activate':
            self.assertEqual(row['phase'],'committed');return dict(phase='activated')
        if operation=='binding':return dict(vm=dict(state='running'))
        if operation=='cleanup':
            self.assertIn(('api','/vm/http-access'),self.events);return dict(phase='cleaned')
        if operation=='prune':return dict(phase='pruned')
        if operation=='discard':
            journal.cancel_attest(self.a,node,identity+':discard',identity,row['revision'],'target-discarded','discarded');return dict(phase='discarded')
        if operation=='restore':
            self.assertIn(('node-two','discard'),self.events)
            journal.cancel_attest(self.a,node,identity+':restore',identity,row['revision'],'source-restored','restored');return dict(phase='restored')
        raise AssertionError(operation)
    def test_whole_coordinator_commits_once_and_hides_private_credentials(self):
        row=self.moves.submit(self.actor,self.body);identity=row['migration_id'];self.moves.run(identity)
        result=self.moves.status(self.actor,identity)
        self.assertEqual(result['job']['status'],'succeeded',result)
        self.assertEqual(self.a.capacity_get('node-two',PROJECT,VM)['node_id'],'node-two')
        self.assertEqual(self.a.quotas(self.customer)['allocated']['max_vms'],1)
        before=list(self.events)
        self.assertEqual(self.moves.submit(self.actor,self.body)['migration_id'],identity);self.assertEqual(self.events,before)
        self.assertNotIn(self.moves.private(identity)['hop']['password'],str(result))
        other=self.a.create_customer('operator','outsider','Other')['customer_id']
        with self.assertRaisesRegex(Failure,'project_membership_required'):self.moves.status(dict(customer=other,agent=None),identity)
    def test_interrupted_export_can_be_cancelled_in_required_order(self):
        self.interrupt=True;identity=self.moves.submit(self.actor,self.body)['migration_id'];self.moves.run(identity)
        self.assertEqual(self.moves.status(self.actor,identity)['job']['status'],'failed')
        self.moves.submit(self.actor,dict(action='cancel',migration_id=identity));self.moves.run(identity)
        self.assertEqual(self.moves.status(self.actor,identity)['job']['status'],'cancelled')
        self.assertLess(self.events.index(('node-two','discard')),self.events.index(('node-one','restore')))
        self.assertEqual(self.a.capacity_get('node-one',PROJECT,VM)['node_id'],'node-one')
if __name__=='__main__':unittest.main()
