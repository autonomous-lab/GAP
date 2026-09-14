import hashlib
from pathlib import Path
import tempfile
import unittest
from authority import Authority, Failure
from access import Access
import suspension
import retention

OWNER='did:gap:'+'a'*64
OTHER='did:gap:'+'b'*64
PROJECT='prj_'+'a'*24
SECOND='prj_'+'b'*24

class SuspensionTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.now=100;self.a=Authority(Path(self.tmp.name)/'db','op',clock=lambda:self.now)
        self.access=Access(self.a,b'k'*32)
        self.first=self.access.connect('one','connect','same@example.com',OWNER,PROJECT)
        self.customer=self.first['customer_id']
        self.access.connect('two','second','SAME@example.com',OTHER,SECOND)
    def change(self,active=True,revision=0,request='suspend',**extra):
        return suspension.set_policy(self.a,request,dict(customer_id=self.customer,active=active,
            expected_revision=revision,reason='Operator abuse decision',**extra))
    def test_account_scope_blocks_both_nodes_and_new_same_email_identity(self):
        actor=self.a.authenticate(self.first['credential']['token']);self.change()
        with self.assertRaises(Failure):self.a.authenticate(self.first['credential']['token'])
        with self.assertRaisesRegex(Failure,'customer_suspended'):self.access.issue(actor,PROJECT)
        with self.assertRaisesRegex(Failure,'customer_suspended'):
            self.access.connect('two','new','same@example.com','did:gap:'+'c'*64,'prj_'+'c'*24)
        s=suspension.snapshot(self.a,'two');self.assertEqual(s['agents'],[OWNER,OTHER])
        self.assertEqual(s['projects'],[PROJECT,SECOND]);self.assertIn(hashlib.sha256(b'same@example.com').hexdigest(),s['email_hashes'])
        with self.assertRaisesRegex(Failure,'customer_suspended'):
            self.a.capacity_prepare('one','create',PROJECT,OWNER,'vm_'+'a'*32,4,1024,0)
    def test_revision_replay_and_restart_preserve_decision(self):
        first=self.change();self.assertEqual(self.change(),first)
        self.a=Authority(self.a.path,'op',clock=lambda:self.now)
        self.assertTrue(suspension.status(self.a,self.customer)['policy']['active'])
        with self.assertRaisesRegex(Failure,'suspension_revision_conflict'):self.change(False,0,'stale')
        self.change(False,1,'restore');self.assertFalse(suspension.snapshot(self.a,'one')['agents'])
        self.assertEqual(len(suspension.status(self.a,self.customer)['history']),2)
    def test_operator_policy_survives_customer_restore(self):
        self.change(scope='operator',request='operator-ban');self.change();self.change(False,1,'restore')
        with self.assertRaisesRegex(Failure,'customer_suspended'):self.a.issue(self.customer,OWNER)
        self.assertTrue(suspension.snapshot(self.a,'one')['all_blocked'])
    def test_abuse_hold_prevents_early_retention_deletion(self):
        with self.a.db() as db:
            db.execute('UPDATE customers SET balance=0 WHERE id=?',(self.customer,))
        retention.status(self.a,'one',PROJECT,OWNER)
        self.change(retention_hours=100);self.now+=73*3600
        with self.assertRaisesRegex(Failure,'retention_not_due'):retention.claim(self.a,'one',PROJECT,OWNER,'purge')
        self.now+=28*3600
        self.assertEqual(retention.claim(self.a,'one',PROJECT,OWNER,'purge')['state'],'claimed')
        with self.assertRaisesRegex(Failure,'retention_already_in_progress'):self.change(True,1,'hold-again')

if __name__=='__main__':unittest.main()
