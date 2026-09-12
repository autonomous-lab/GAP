import unittest
from types import SimpleNamespace
from billing import BillingError
from admission import check

class AdmissionTests(unittest.TestCase):
    def fixture(self):
        price={'version':'test','vcpu_hour':10}
        return SimpleNamespace(config={'operator_id':'op','node_id':'node','expected_tariff':price},
            pricing=lambda:dict(mode='enforced',tariff=price),
            transport=lambda body:dict(operator_id='op',node_id='node',protocol='fleet-admission-v1',reservations=True,capacity=True))

    def test_tariff_and_billing_fail_before_readiness(self):
        ledger=self.fixture();ledger.pricing=lambda:dict(mode='shadow',tariff=None)
        self.assertEqual(check(ledger)['errors'],['fleet_requires_enforced_billing','fleet_active_tariff_mismatch'])
        ledger=self.fixture();self.assertTrue(check(ledger)['ready'])
        ledger.config.pop('expected_tariff');self.assertIn('fleet_expected_tariff_not_configured',check(ledger)['errors'])

    def test_authority_outage_identity_and_protocol(self):
        ledger=self.fixture()
        def offline(body):raise BillingError('fleet_authority_unavailable')
        ledger.transport=offline;self.assertEqual(check(ledger)['errors'],['fleet_authority_unavailable'])
        ledger.transport=lambda body:dict(operator_id='other',node_id='node')
        self.assertFalse(check(ledger)['ready']);self.assertIn('fleet_readiness_identity_mismatch',check(ledger)['errors'])

    def test_failed_admission_inserts_no_job_but_cleanup_still_queues(self):
        import tempfile,sqlite3,threading
        from pathlib import Path
        from unittest.mock import patch
        from runner import Runner,Failure
        with tempfile.TemporaryDirectory() as directory:
            runner=Runner.__new__(Runner);runner.db_path=Path(directory)/'jobs.sqlite';runner.lock=threading.Lock()
            with runner.db() as db:
                db.execute("CREATE TABLE jobs(id TEXT,project TEXT,owner TEXT,request TEXT,digest TEXT,action TEXT,payload TEXT,status TEXT,result TEXT,created INTEGER)")
            runner.authorize=lambda *args:None
            runner.hypervisor=object();ledger=self.fixture();ledger.pricing=lambda:dict(mode='shadow',tariff=None)
            runner.runtime=SimpleNamespace(ledger=ledger)
            rpc=dict(project_id='prj_'+'a'*24,owner_did='owner',method='POST',action='vm/create',body={'request_id':'a'*32})
            with patch('microvm.validate'),patch('runner.threading.Thread'):
                with self.assertRaises(Failure) as failure:runner.rpc(rpc)
                self.assertEqual(failure.exception.status,503)
                with runner.db() as db:self.assertEqual(db.execute('SELECT count(*) FROM jobs').fetchone()[0],0)
                rpc['action']='vm/destroy'
                self.assertEqual(runner.rpc(rpc)[0],202)

    def test_legacy_local_is_unaffected(self):
        self.assertTrue(check(SimpleNamespace())['ready'])

if __name__=='__main__':unittest.main()
