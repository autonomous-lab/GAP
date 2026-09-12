import unittest
import retention
from test_authority import OWNER, PROJECT
from authority import Failure

class RetentionTests(unittest.TestCase):
    def setUp(self):
        import tempfile
        from pathlib import Path
        from authority import Authority
        t=tempfile.TemporaryDirectory();self.addCleanup(t.cleanup)
        self.now=100;self.a=Authority(Path(t.name)/'a.db','op',lambda:self.now)
        self.c=self.a.create_customer('operator','c','Customer')['customer_id']
        self.a.attach_principal('operator','o',self.c,'agent',OWNER)
        self.a.attach_project('operator','p',self.c,PROJECT,'node',OWNER)
    def status(self):return retention.status(self.a,'node',PROJECT,OWNER)
    def test_deadline_topup_and_atomic_claim(self):
        self.assertEqual(self.status()['delete_after'],100+retention.SECONDS)
        self.now+=retention.SECONDS
        self.a.topup('operator','topup',self.c,100,'promotional')
        self.assertIsNone(self.status()['delete_after'])
        with self.assertRaises(Failure):retention.claim(self.a,'node',PROJECT,OWNER,'vm-one')
        self.a.checkpoint('node','r1',PROJECT,OWNER,'rsv',0,0,100,10)
        self.assertIsNone(self.status()['delete_after'])
        self.a.checkpoint('node','r2',PROJECT,OWNER,'rsv',100,0,100,10)
        self.status();self.now+=retention.SECONDS
        self.assertEqual(retention.claim(self.a,'node',PROJECT,OWNER,'vm-one')['state'],'claimed')
        self.a.topup('operator','late',self.c,100,'promotional')
        reply=self.a.checkpoint('node','r3',PROJECT,OWNER,'rsv',100,0,100,10)
        self.assertEqual(reply['lease_expires_at'],0)
        self.assertEqual(reply['allocated_microcredits'],100)
        self.assertEqual(retention.claim(self.a,'node',PROJECT,OWNER,'vm-one')['state'],'claimed')
        retention.finish(self.a,'node',PROJECT,OWNER,'vm-one')
        reply=self.a.checkpoint('node','r4',PROJECT,OWNER,'rsv',100,0,100,10)
        self.assertGreater(reply['lease_expires_at'],self.now)
    def test_unknown_node_and_owner_refused(self):
        with self.assertRaises(Failure):retention.status(self.a,'other',PROJECT,OWNER)
        with self.assertRaises(Failure):retention.claim(self.a,'node',PROJECT,'someone','claim')
