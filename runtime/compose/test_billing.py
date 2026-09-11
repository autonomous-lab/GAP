import json
from pathlib import Path
import tempfile
import unittest
from billing import Ledger, BillingError, GIB, RETENTION_SECONDS

P='prj_'+'a'*24
O='did:gap:'+'b'*64
V='vm_'+'c'*32


class LedgerTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.now=1000.0
        self.ledger=Ledger(Path(self.temp.name)/'credits.sqlite',lambda:self.now)
        self.meta={'project_id':P,'owner_did':O,'vm_id':V,'vcpus':1,'memory_mib':1024}
        self.price={'version':'test-1','vcpu_hour':3600,'gib_ram_hour':3600,
                    'gib_disk_hour':3600,'gib_in':100,'gib_out':200}

    def sample(self,ms,on=True,disk=GIB,incoming=0,outgoing=0,incarnation='a'):
        self.now=ms/1000
        self.ledger.sample(self.meta,ms,on,disk,incoming,outgoing,incarnation)

    def test_shadow_does_not_debit_and_tariff_versions_are_immutable(self):
        self.ledger.set_pricing('shadow',self.price)
        self.ledger.topup(P,O,100,'one')
        self.sample(1000);self.sample(2000)
        view=self.ledger.view(P,O)
        self.assertEqual(view['balance_microcredits'],100)
        self.assertEqual(view['estimated_microcredits'],3)
        with self.assertRaisesRegex(BillingError,'immutable'):
            self.ledger.set_pricing('shadow',dict(self.price,vcpu_hour=1))

    def test_metering_debit_and_checkpoint_are_atomic_and_idempotent(self):
        self.ledger.set_pricing('enforced',self.price)
        self.ledger.topup(P,O,1000,'one')
        self.sample(1000);self.sample(2000,incoming=GIB,outgoing=GIB)
        self.sample(2000,incoming=GIB,outgoing=GIB)
        view=self.ledger.view(P,O)
        self.assertEqual(view['spent_microcredits'],303)
        self.assertEqual(view['balance_microcredits'],697)
        self.assertEqual(len([e for e in view['entries'] if e['kind']=='usage']),1)
        self.ledger=Ledger(self.ledger.path,lambda:self.now)
        self.sample(3000,incoming=GIB,outgoing=GIB,incarnation='new')
        self.assertEqual(self.ledger.view(P,O)['spent_microcredits'],304) # only disk across worker outage

    def test_subsecond_usage_carries_fractional_credits(self):
        self.ledger.set_pricing('enforced',self.price);self.ledger.topup(P,O,100,'one')
        self.sample(1000)
        for stamp in range(1100,2100,100): self.sample(stamp)
        self.assertEqual(self.ledger.view(P,O)['spent_microcredits'],3)

    def test_topup_dedup_conflict_owner_and_retention_recharge(self):
        self.ledger.set_pricing('enforced',self.price)
        self.ledger.topup(P,O,3,'one'); self.ledger.topup(P,O,3,'one')
        with self.assertRaises(BillingError): self.ledger.topup(P,O,4,'one')
        with self.assertRaises(BillingError): self.ledger.view(P,'did:gap:'+'d'*64)
        self.sample(1000);self.sample(2000)
        view=self.ledger.view(P,O)
        self.assertEqual(view['delete_after'],2+RETENTION_SECONDS)
        self.now+=RETENTION_SECONDS-1
        self.assertFalse(self.ledger.claim_expired(P,O,V))
        self.ledger.topup(P,O,10,'recharge')
        self.assertIsNone(self.ledger.view(P,O)['delete_after'])
        self.now+=100
        self.assertFalse(self.ledger.claim_expired(P,O,V))

    def test_deletion_claim_wins_over_late_recharge(self):
        self.ledger.set_pricing('enforced',self.price)
        self.sample(1000);self.sample(2000)
        self.now=2+RETENTION_SECONDS
        self.assertTrue(self.ledger.claim_expired(P,O,V))
        self.assertTrue(self.ledger.claim_expired(P,O,V))
        with self.assertRaisesRegex(BillingError,'deletion_already'):
            self.ledger.topup(P,O,10,'too-late')
        self.ledger.finish_deletion(P,O,V)
        self.ledger.topup(P,O,10,'new-vm')

    def test_budget_stops_execution_but_storage_remains_billable(self):
        self.ledger.set_pricing('enforced',self.price);self.ledger.topup(P,O,100,'one')
        self.ledger.set_budget(P,O,2,'budget-one')
        self.sample(1000);self.sample(2000)
        view=self.ledger.view(P,O)
        self.assertEqual(view['spent_microcredits'],3)
        self.assertFalse(view['execution_allowed'])
        self.assertIsNone(view['delete_after'])
        self.sample(3000,on=False)
        self.sample(4000,on=False)
        self.assertEqual(self.ledger.view(P,O)['spent_microcredits'],7)
        self.ledger.set_budget(P,O,10,'budget-two')
        self.assertTrue(self.ledger.view(P,O)['execution_allowed'])


if __name__=='__main__': unittest.main()
