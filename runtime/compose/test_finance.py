import json
from pathlib import Path
import tempfile
import unittest
from billing import Ledger, BillingError
import finance

P='prj_'+'a'*24
O='did:gap:'+'b'*64


class FinanceTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.now=0
        self.path=Path(self.temp.name)/'ledger.sqlite'
        self.ledger=Ledger(self.path,clock=lambda:self.now)
    def tearDown(self):self.temp.cleanup()
    def costs(self,version='cost-v1',network=0):
        return dict(version=version,node_month_microdollars=55_000_000,
                    extra_disk_month_microdollars=0,network_in_gb_microdollars=network,
                    network_out_gb_microdollars=network)
    def test_project_summaries_preserve_funding_and_do_not_change_balance(self):
        self.ledger.topup(P,O,100,'funding')
        before=self.ledger.view(P,O)['balance_microcredits']
        report=self.ledger.finance_report(0,3600,include_projects=True)
        self.assertTrue(report['projects_complete'])
        self.assertEqual(report['projects'][0]['funding'],report['funding'])
        self.assertEqual(report['projects'][0]['lifetime_spent_microcredits'],0)
        self.assertEqual(self.ledger.view(P,O)['balance_microcredits'],before)

    def test_hour_split_conserves_big_resource_and_money_totals(self):
        item={'usage':{'ram_byte_ms':4*1024**3*7200000,'bytes_in':11},'debited_microcredits':7}
        with self.ledger.db() as db:finance.record(db,P,item,3599,7201)
        report=self.ledger.finance_report(0,10800)
        self.assertEqual(sum(v['debited_microcredits'] for v in report['hours']),7)
        self.assertEqual(report['usage']['ram_byte_ms'],item['usage']['ram_byte_ms'])
        self.assertEqual(report['usage']['bytes_in'],11)
        self.assertIsNone(report['usage_margin_microdollars'])
        self.assertFalse(report['cost_complete'])
    def test_fixed_cost_rounding_conserves_month_and_project_is_not_charged_whole_node(self):
        self.ledger.finance_costs(self.costs(),None)
        r=self.ledger.finance_report(0,730*3600)
        self.assertEqual(r['known_infra_cost_microdollars'],55_000_000)
        self.assertEqual(sum(h['known_infra_cost_microdollars'] for h in r['hours']),55_000_000)
        self.assertTrue(r['cost_complete'])
        self.assertEqual(r['usage_margin_microdollars'],-55_000_000)
        project=self.ledger.finance_report(0,3600,P)
        self.assertIsNone(project['known_infra_cost_microdollars'])
        self.assertIsNone(project['hours'][0]['known_infra_cost_microdollars'])
    def test_unknown_network_costs_never_become_zero_and_versions_are_immutable(self):
        self.ledger.finance_costs(self.costs(network=None),None)
        r=self.ledger.finance_report(0,3600)
        self.assertFalse(r['cost_complete']);self.assertIsNone(r['usage_margin_microdollars'])
        with self.assertRaisesRegex(BillingError,'immutable_cost_version'):
            self.ledger.finance_costs(self.costs(),'cost-v1')
        with self.assertRaisesRegex(BillingError,'cost_version_changed'):
            self.ledger.finance_costs(self.costs('cost-v2'),None)
        self.now=1800
        self.ledger.finance_costs(self.costs('cost-v2'),'cost-v1')
        self.assertFalse(self.ledger.finance_report(0,3600)['cost_complete'])
        self.assertTrue(self.ledger.finance_report(3600,7200)['cost_complete'])
    def test_promotion_is_not_cash_and_annotation_does_not_change_wallet(self):
        self.ledger.topup(P,O,100_000_000,'gift')
        before=self.ledger.view(P,O)
        self.assertEqual(self.ledger.finance_report(0,3600)['funding']['unclassified_credits_microcredits'],100_000_000)
        self.ledger.classify_funding(P,'topup:gift','promotional',0,'Owner allocation')
        self.ledger.classify_funding(P,'topup:gift','promotional',0,'Owner allocation')
        r=self.ledger.finance_report(0,3600)
        self.assertEqual(r['funding']['promotional_credits_microcredits'],100_000_000)
        self.assertEqual(r['funding']['recorded_cash_microdollars'],0)
        self.assertEqual(self.ledger.view(P,O),before)
        with self.assertRaisesRegex(BillingError,'immutable_funding'):
            self.ledger.classify_funding(P,'topup:gift','paid',100_000_000,'Changed')
    def test_backfill_and_reopening_never_duplicate_debits(self):
        self.ledger.topup(P,O,100,'gift')
        with self.ledger.db() as db:
            item={'kind':'usage','usage':{'bytes_out':19},'debited_microcredits':7,'started_at':3500,'ended_at':7300}
            db.execute('INSERT INTO entries(project,operation,digest,payload,created) VALUES(?,?,?,?,?)',(P,'legacy','digest',json.dumps(item),7300))
            db.execute("DELETE FROM finance_meta WHERE key='initialized_at'")
        self.ledger=Ledger(self.path,clock=lambda:self.now)
        a=self.ledger.finance_report(0,10800)
        self.ledger=Ledger(self.path,clock=lambda:self.now)
        self.assertEqual(a,self.ledger.finance_report(0,10800))
        self.assertEqual(a['usage']['debited_microcredits'],7)
        self.assertTrue(a['historical_apportionment'])
        self.assertEqual(self.ledger.view(P,O)['balance_microcredits'],100)
    def test_meter_samples_reconcile_exactly_after_retries_and_reopening(self):
        tariff=dict(version='usd-v1',vcpu_hour=10000,gib_ram_hour=0,gb_disk_month=0,gb_in=0,gb_out=0)
        self.ledger.set_pricing('enforced',tariff)
        self.ledger.topup(P,O,1000000,'gift')
        meta=dict(vm_id='vm_'+'c'*32,project_id=P,owner_did=O,vcpus=1,memory_mib=1024,state='running')
        for hour in range(13):self.ledger.sample(meta,hour*3600000,True,0,0,0,'worker')
        self.ledger.sample(meta,12*3600000,True,0,0,0,'worker')
        report=self.ledger.finance_report(0,13*3600)
        self.assertEqual(report['usage']['debited_microcredits'],120000)
        self.assertEqual(report['usage']['debited_microcredits'],self.ledger.view(P,O)['spent_microcredits'])
        self.ledger=Ledger(self.path,clock=lambda:self.now)
        self.assertEqual(report,self.ledger.finance_report(0,13*3600))

    def test_invalid_report_windows_are_rejected(self):
        for start,end in [(1,3600),(3600,0),(0,3601),(0,367*24*3600)]:
            with self.assertRaises(BillingError):self.ledger.finance_report(start,end)


if __name__=='__main__':unittest.main()
