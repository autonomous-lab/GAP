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

    def test_fractional_cpu_keeps_submillisecond_carry(self):
        self.meta['vcpus'] = .25
        self.sample(0, disk=0)
        for stamp in range(1,1001): self.sample(stamp, disk=0)
        view=self.ledger.view(P,O)
        used=sum(e.get('usage',{}).get('vcpu_ms',0) for e in view['entries'])
        self.assertEqual(used,250)

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

    def test_tariff_compare_and_set_preserves_concurrent_operator_update(self):
        self.ledger.set_pricing('enforced',self.price,expected_version=None)
        updated=dict(self.price,version='test-2',vcpu_hour=7200)
        self.ledger.set_pricing('enforced',updated,expected_version='test-1')
        for expected in (None,'test-1'):
            with self.assertRaisesRegex(BillingError,'tariff_version_changed'):
                self.ledger.set_pricing('enforced',dict(self.price,version='stale'),expected_version=expected)
        self.assertEqual(self.ledger.pricing()['tariff'],updated)
        with self.ledger.db() as db:
            self.assertIsNone(db.execute("SELECT version FROM tariffs WHERE version='stale'").fetchone())

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

    def test_period_accumulates_until_state_or_price_changes(self):
        self.ledger.set_pricing('enforced',self.price);self.ledger.topup(P,O,10000,'one')
        self.meta['state']='running'
        self.sample(0)
        for stamp in range(1000,101000,1000): self.sample(stamp)
        rows=[e for e in self.ledger.view(P,O)['entries'] if e['kind']=='usage']
        self.assertEqual(len(rows),1)
        self.assertEqual(rows[0]['usage']['vcpu_ms'],100000)
        self.assertEqual(rows[0]['debited_microcredits'],300)
        self.assertEqual(rows[0]['sample_count'],100)
        self.meta['state']='hibernated'
        self.sample(100000,on=False)  # transition at the same checkpoint timestamp
        self.sample(101000,on=False)
        rows=[e for e in self.ledger.view(P,O)['entries'] if e['kind']=='usage']
        self.assertEqual([e['state'] for e in rows],['hibernated','running'])
        self.assertEqual(rows[0]['debited_microcredits'],1)
        self.ledger.set_pricing('enforced',dict(self.price,version='test-2'))
        self.sample(102000,on=False)
        rows=[e for e in self.ledger.view(P,O)['entries'] if e['kind']=='usage']
        self.assertEqual(len(rows),3)
        self.assertEqual(rows[0]['tariff_version'],'test-2')
        self.assertEqual(sum(e['debited_microcredits'] for e in rows),self.ledger.view(P,O)['spent_microcredits'])

    def test_legacy_compaction_preserves_raw_audit_and_never_changes_balance(self):
        self.ledger.set_pricing('enforced',self.price);self.ledger.topup(P,O,1000,'one')
        for n,on in enumerate([True,True,False],1):
            usage={'vcpu_ms':1000 if on else 0,'ram_byte_ms':GIB*1000 if on else 0,
                   'disk_byte_ms':GIB*1000,'bytes_in':0,'bytes_out':0}
            self.now=n
            with self.ledger.db() as db:self.ledger._charge(db,P,O,f'meter:{V}:{n*1000}',usage,'enforced',self.price)
        before=self.ledger.view(P,O)
        self.ledger=Ledger(self.ledger.path,lambda:self.now)
        after=self.ledger.view(P,O)
        self.assertEqual(after['balance_microcredits'],before['balance_microcredits'])
        self.assertEqual(after['spent_microcredits'],before['spent_microcredits'])
        rows=[e for e in after['entries'] if e['kind']=='usage']
        self.assertEqual(len(rows),2)
        self.assertEqual(sum(e['debited_microcredits'] for e in rows),7)
        self.assertTrue(all(e['historical'] for e in rows))
        with self.ledger.db() as db:self.assertEqual(db.execute('SELECT count(*) FROM legacy_meter_entries').fetchone()[0],3)
        self.ledger=Ledger(self.ledger.path,lambda:self.now)
        self.assertEqual(self.ledger.view(P,O),after)

    def test_period_update_and_charge_roll_back_with_checkpoint(self):
        from unittest.mock import patch
        self.ledger.set_pricing('enforced',self.price);self.ledger.topup(P,O,1000,'one')
        self.sample(0);self.sample(1000)
        original=self.ledger.accumulate
        def fail(*args):
            original(*args)
            raise RuntimeError('simulated interrupted transaction')
        with patch.object(self.ledger,'accumulate',side_effect=fail):
            with self.assertRaises(RuntimeError):self.sample(2000)
        self.assertEqual(self.ledger.view(P,O)['spent_microcredits'],3)
        self.sample(2000)
        rows=[e for e in self.ledger.view(P,O)['entries'] if e['kind']=='usage']
        self.assertEqual(len(rows),1)
        self.assertEqual(rows[0]['debited_microcredits'],6)

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

    def test_commercial_gb_month_and_network_rates_are_exact_across_restarts(self):
        price={'version':'usd-v1','vcpu_hour':10000,'gib_ram_hour':10000,
               'gb_disk_month':100000,'gb_in':10000,'gb_out':10000}
        self.ledger.set_pricing('enforced',price)
        self.ledger.topup(P,O,100_000_000,'authorized')
        self.sample(0,on=False,disk=10**9)
        # One decimal GB over 730 hours, split into hourly checkpoints.
        for hour in range(1,731):
            self.sample(hour*3_600_000,on=False,disk=10**9,
                        incoming=hour*10**9//730,outgoing=hour*10**9//730)
            if hour==365:
                self.ledger=Ledger(self.ledger.path,lambda:self.now)
        self.assertEqual(self.ledger.view(P,O)['spent_microcredits'],120000)
        self.assertEqual(self.ledger.view(P,O)['balance_microcredits'],99_880_000)

    def test_fractional_carry_survives_tariff_unit_change(self):
        commercial={'version':'usd-v1','vcpu_hour':10000,'gib_ram_hour':10000,
                    'gb_disk_month':100000,'gb_in':10000,'gb_out':10000}
        self.ledger.set_pricing('enforced',commercial)
        self.ledger.topup(P,O,1000,'authorized')
        self.sample(0,on=False,disk=10**9)
        self.sample(1,on=False,disk=10**9)
        self.ledger.set_pricing('enforced',self.price)
        self.sample(2,on=False,disk=10**9)
        self.ledger.set_pricing('enforced',dict(commercial,version='usd-v2'))
        self.sample(730*3_600_000+1,on=False,disk=10**9)
        view=self.ledger.view(P,O)
        self.assertEqual(view['estimated_microcredits'],100000)

    def test_operator_recharge_checkpoints_grace_usage_before_new_balance(self):
        from contextlib import nullcontext
        from types import SimpleNamespace
        from runner import Runner
        self.ledger.set_pricing('enforced',self.price)
        self.sample(1000,on=False)
        runner=Runner.__new__(Runner)
        runner.authorize=lambda *_: {}
        runner.hypervisor=SimpleNamespace(read=lambda *_:self.meta,list=lambda *_:[self.meta])
        runner.runtime=SimpleNamespace(ledger=self.ledger,lock=lambda _:nullcontext(),sample=lambda _:self.sample(6000,on=False))
        runner.operator({'action':'topup','project_id':P,'owner_did':O,'amount_microcredits':100,'request_id':'paid'})
        self.assertEqual(self.ledger.view(P,O)['balance_microcredits'],100)
        self.sample(7000,on=False)
        self.assertEqual(self.ledger.view(P,O)['spent_microcredits'],1)

    def test_budget_reset_checkpoints_previous_spending_period(self):
        from contextlib import nullcontext
        from types import SimpleNamespace
        from runner import Runner
        self.ledger.set_pricing('enforced',self.price);self.ledger.topup(P,O,100,'paid')
        self.sample(1000)
        runner=Runner.__new__(Runner)
        runner.authorize=lambda *_:{}
        runner.hypervisor=SimpleNamespace(read=lambda *_:self.meta,list=lambda *_:[self.meta])
        runner.runtime=SimpleNamespace(ledger=self.ledger,lock=lambda _:nullcontext(),sample=lambda _:self.sample(2000))
        runner.rpc({'project_id':P,'owner_did':O,'action':'budget','method':'PUT','body':{'request_id':'new-period','budget_microcredits':10}})
        self.assertEqual(self.ledger.view(P,O)['budget_spent_microcredits'],0)
        self.sample(3000)
        self.assertEqual(self.ledger.view(P,O)['budget_spent_microcredits'],3)


if __name__=='__main__': unittest.main()
