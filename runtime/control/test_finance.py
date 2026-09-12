import copy
from pathlib import Path
import tempfile
import unittest
from authority import Authority,Failure
from finance import Finance,FIELDS,FUNDING
from service import Application
from test_authority import OWNER,PROJECT,SECOND


class FinanceTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup);root=Path(self.temp.name)
        self.a=Authority(root/'control.sqlite','one',clock=lambda:100)
        self.customer=self.a.create_customer('operator','customer','Customer')['customer_id']
        self.a.attach_principal('operator','owner',self.customer,'agent',OWNER)
        self.a.attach_project('operator','p1',self.customer,PROJECT,'node-one',OWNER)
        self.a.attach_project('operator','p2',self.customer,SECOND,'node-two',OWNER)
        self.a.topup('operator','paid',self.customer,1000,'paid')
        self.a.checkpoint('node-one','r1',PROJECT,OWNER,'reservation1',0,0,100,30)
        self.a.checkpoint('node-two','r2',SECOND,OWNER,'reservation2',0,0,100,30)
        with self.a.db() as db:self.a.entry(db,self.customer,None,None,'wallet_import',999,'migration','operator','import')
        self.a.clock=lambda:10000
        self.reports={}
        sources=[]
        for node,project,charge,cost in [('node-one',PROJECT,10,5),('node-two',SECOND,20,7)]:
            token=root/(node+'.token');token.write_text('a'*64)
            sources.append(dict(node_id=node,provider='Provider',url='https://example.invalid/v1/fleet/node-finance',token_file=str(token)))
            usage={k:0 for k in FIELDS};usage['debited_microcredits']=charge
            funding={k:0 for k in FUNDING};funding['paid_credits_microcredits']=100;funding['recorded_cash_microdollars']=100
            self.reports[node]=dict(available=True,start=0,end=7200,project_id=None,usage=usage,funding=funding,known_infra_cost_microdollars=cost,cost_complete=True,historical_apportionment=False,
                hours=[dict(hour=0,**usage,known_infra_cost_microdollars=cost,cost_complete=True),dict(hour=3600,**{k:0 for k in FIELDS},known_infra_cost_microdollars=0,cost_complete=True)],
                projects=[dict(project_id=project,usage=usage,funding=funding,lifetime_spent_microcredits=charge,fleet_bound=True)],projects_complete=True)
        def fetch(source,*args):
            result=self.reports[source['node_id']]
            if result is None:raise OSError('offline')
            return copy.deepcopy(result)
        self.finance=Finance(self.a,sources,['node-one','node-two'],transport=fetch)

    def test_complete_costs_and_wallet_count_reservations_once(self):
        r=self.finance.report(0,7200)
        self.assertEqual(r['usage']['debited_microcredits'],30)
        self.assertEqual(r['usage_margin_microdollars'],18)
        self.assertEqual(r['funding']['paid_credits_microcredits'],1200)
        self.assertEqual(r['wallet_totals']['balance_microcredits'],800)
        self.assertEqual(r['wallet_totals']['reserved_microcredits'],200)
        self.assertEqual(r['wallet_totals']['total_remaining_microcredits'],1000)
        self.assertEqual(r['customers'][0]['unsettled_usage_microcredits'],30)
        self.assertFalse(r['cash_receipts_complete'])

    def test_unavailable_node_and_unknown_costs_never_produce_margin(self):
        self.reports['node-two']=None
        r=self.finance.report(0,7200)
        self.assertFalse(r['coverage_complete']);self.assertIsNone(r['usage_margin_microdollars'])
        self.assertEqual(r['known_infra_cost_microdollars'],5)
        self.assertIsNone(r['customers'][0]['unsettled_usage_microcredits'])
        self.reports['node-two']=copy.deepcopy(self.reports['node-one'])
        self.reports['node-two']['cost_complete']=False
        r=self.finance.report(0,7200)
        self.assertTrue(r['coverage_complete']);self.assertIsNone(r['usage_margin_microdollars'])

    def test_customer_filter_never_allocates_entire_host_cost(self):
        r=self.finance.report(0,7200,customer=self.customer)
        self.assertEqual(r['usage']['debited_microcredits'],30)
        self.assertIsNone(r['known_infra_cost_microdollars']);self.assertIsNone(r['usage_margin_microdollars'])
        self.assertEqual(r['hours'],[])
        r=self.finance.report(0,7200,node='node-one')
        self.assertEqual(r['usage']['debited_microcredits'],10)
        self.assertEqual(r['funding']['paid_credits_microcredits'],100)
        self.assertEqual(r['wallet_totals']['total_remaining_microcredits'],1000)
        with self.assertRaises(Failure):self.finance.report(1,7200)

    def test_reporting_credential_cannot_mutate_or_read_client_wallet_api(self):
        app=Application(self.a,'admin',{'node-one':'worker1','node-two':'worker2'},finance=self.finance,report_token='report')
        human=self.a.issue(self.customer,None)['token']
        for token in [human,'worker1']:
            with self.assertRaisesRegex(Failure,'finance_operator_required'):app.handle('GET','/v1/finance?start=0&end=7200',token,None)
        self.assertEqual(app.handle('GET','/v1/finance?start=0&end=7200','report',None)['usage']['debited_microcredits'],30)
        with self.assertRaisesRegex(Failure,'operator_credentials_required'):app.handle('POST','/operator','report',{'action':'create-customer'})
        with self.assertRaisesRegex(Failure,'client_credentials_required'):app.handle('GET','/v1/wallet','report',None)

if __name__=='__main__':unittest.main()
