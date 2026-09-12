import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0,os.environ.get('GAP_TEST_CONTROL',str(Path(__file__).resolve().parents[1]/'control')))
from authority import Authority
from billing import Ledger,BillingError,UNITS,RETENTION_SECONDS
from fleet_billing import FleetLedger

P='prj_'+'a'*24
O='did:gap:'+'b'*64
PRICE={'version':'fleet-test','vcpu_hour':3600000,'gib_ram_hour':0,'gib_disk_hour':0,'gib_in':0,'gib_out':0}


class FleetTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        root=Path(self.temp.name);self.now=1000;self.mono=50
        self.authority=Authority(root/'authority.sqlite','operator',lambda:self.now)
        self.customer=self.authority.create_customer('operator','customer','Test')['customer_id']
        self.authority.attach_principal('operator','owner',self.customer,'agent',O)
        self.authority.attach_project('operator','project',self.customer,P,'node',O)
        self.authority.topup('operator','fund',self.customer,10000,'promotional')
        self.path=root/'ledger.sqlite'
        Ledger(self.path).set_pricing('enforced',PRICE)
        self.config=dict(projects=[P],operator_id='operator',node_id='node',target_microcredits=100,lease_seconds=10)
        self.down=False;self.lose_reply=False
        self.ledger=self.open()

    def transport(self,body):
        if self.down:raise BillingError('fleet_authority_unavailable')
        reply=self.authority.checkpoint('node',body['request_id'],P,O,body['reservation_id'],body['consumed_microcredits'],
              body['unpaid_microcredits'],body['target_microcredits'],body['lease_seconds'])
        if self.lose_reply:raise BillingError('lost_reply')
        return dict(reply,authority_now=self.now)

    def open(self):
        return FleetLedger(self.path,self.config,lambda:self.now,lambda:self.mono,self.transport)

    def advance(self,seconds):self.now+=seconds;self.mono+=seconds

    def charge(self,value,key='usage'):
        with self.ledger.db() as db:
            self.ledger._charge(db,P,O,key,dict.fromkeys(UNITS,0)|{'vcpu_ms':value},'enforced',PRICE)

    def test_allowance_is_settled_once_and_refilled_without_copying_wallet(self):
        self.ledger.sync(P,O,True)
        self.assertEqual(self.ledger.view(P,O)['balance_microcredits'],100)
        self.charge(80)
        self.ledger.sync(P,O,True)
        self.assertEqual(self.ledger.view(P,O)['balance_microcredits'],100)
        central=self.authority.wallet(self.customer)
        self.assertEqual((central['spent_microcredits'],central['reserved_microcredits']),(80,100))
        self.assertEqual(central['balance_microcredits'],9820)

    def test_outage_expires_monotonically_and_never_triggers_retention(self):
        self.ledger.sync(P,O,True)
        self.down=True
        self.advance(5);self.ledger.sync(P,O,True)
        self.assertTrue(self.ledger.lease_allowed(P))
        self.now-=1000000;self.mono+=6
        self.assertFalse(self.ledger.lease_allowed(P))
        self.assertFalse(self.ledger.view(P,O)['execution_allowed'])
        self.advance(RETENTION_SECONDS+1)
        view=self.ledger.view(P,O)
        self.assertIsNone(view['delete_after'])
        self.assertFalse(self.ledger.claim_expired(P,O,'vm_test'))
        self.assertEqual(view['balance_microcredits'],100)

    def test_lost_reply_and_restart_reuse_outbox_without_new_lease_from_old_reply(self):
        self.lose_reply=True;self.ledger.sync(P,O,True)
        self.assertFalse(self.ledger.lease_allowed(P))
        self.assertEqual(self.authority.wallet(self.customer)['reserved_microcredits'],100)
        self.advance(11);self.lose_reply=False;self.ledger=self.open()
        self.ledger.sync(P,O,True)
        self.assertFalse(self.ledger.lease_allowed(P))
        self.assertEqual(self.ledger.view(P,O)['balance_microcredits'],100)
        self.ledger.sync(P,O,True)
        self.assertTrue(self.ledger.lease_allowed(P))
        self.assertEqual(self.authority.wallet(self.customer)['reserved_microcredits'],100)

    def test_restart_during_partition_cannot_reuse_a_persisted_lease(self):
        self.ledger.sync(P,O,True)
        self.down=True;self.ledger=self.open()
        self.ledger.sync(P,O,True)
        self.assertFalse(self.ledger.view(P,O)['execution_allowed'])

    def test_stale_outbox_and_new_local_usage_reconcile_exactly(self):
        self.ledger.sync(P,O,True)
        self.charge(20,'first');self.lose_reply=True;self.ledger.sync(P,O,True)
        self.charge(30,'second');self.lose_reply=False;self.ledger.sync(P,O,True)
        self.assertEqual(self.ledger.view(P,O)['balance_microcredits'],70)
        self.ledger.sync(P,O,True)
        self.assertEqual(self.ledger.view(P,O)['balance_microcredits'],100)
        self.assertEqual(self.authority.wallet(self.customer)['spent_microcredits'],50)

    def test_unpaid_storage_is_repaid_before_new_execution_and_finance_conserves_debits(self):
        self.ledger.sync(P,O,True)
        self.charge(150)
        self.assertFalse(self.ledger.lease_allowed(P))
        self.down=True;self.ledger.sync(P,O,True)
        self.advance(RETENTION_SECONDS+1)
        self.assertFalse(self.ledger.claim_expired(P,O,'vm_test'))
        self.down=False;self.ledger.sync(P,O,True)
        view=self.ledger.view(P,O)
        self.assertEqual((view['balance_microcredits'],view['spent_microcredits']),(50,150))
        self.ledger.sync(P,O,True)
        self.assertEqual(self.authority.wallet(self.customer)['spent_microcredits'],150)
        with self.ledger.db() as db:
            hours=[json.loads(r[0]) for r in db.execute('SELECT data FROM finance_hours')]
        self.assertEqual(sum(r['debited_microcredits'] for r in hours),150)
        self.assertEqual(sum(r['unpaid_microcredits'] for r in hours),0)

    def test_legacy_wallet_and_local_topup_cannot_create_a_second_spending_path(self):
        self.ledger.sync(P,O,True)
        with self.assertRaisesRegex(BillingError,'use_authoritative_customer_wallet'):
            self.ledger.topup(P,O,100,'wrong-path')
        with self.assertRaisesRegex(BillingError,'fleet_requires_enforced_billing'):
            self.ledger.set_pricing('shadow',PRICE)
        local=Ledger(self.path)
        self.assertFalse(local.lease_allowed(P))
        with self.assertRaisesRegex(BillingError,'fleet_configuration_required'):local.view(P,O)
        other=Path(self.temp.name)/'legacy.sqlite'
        old=Ledger(other);old.set_pricing('enforced',PRICE);old.topup(P,O,50,'legacy')
        managed=FleetLedger(other,self.config,transport=self.transport)
        with self.assertRaisesRegex(BillingError,'legacy_wallet_migration_required'):managed.sync(P,O,True)
        self.assertEqual(old.view(P,O)['balance_microcredits'],50)
        changed=FleetLedger(self.path,dict(self.config,operator_id='another-operator'),transport=self.transport)
        with self.assertRaisesRegex(BillingError,'fleet_authority_binding_mismatch'):changed.sync(P,O,True)

    def test_paused_cpu_is_not_charged_until_the_next_meter_tick(self):
        self.ledger.sync(P,O,True)
        meta=dict(project_id=P,owner_did=O,vm_id='vm_'+'c'*32,vcpus=1,memory_mib=1024,state='running')
        self.ledger.sample(meta,1000,True,0,0,0,'boot')
        self.ledger.sample(dict(meta,meter_stop_ms=1010),2000,False,0,0,0,'boot')
        self.assertEqual(self.ledger.view(P,O)['estimated_microcredits'],10)

    def test_confirmed_empty_wallet_is_distinct_from_controller_unavailability(self):
        with self.authority.db() as db:db.execute('UPDATE customers SET balance=0 WHERE id=?',(self.customer,))
        self.ledger.sync(P,O,True)
        first=self.ledger.view(P,O)
        self.assertEqual(first['fleet']['funding_status'],'exhausted')
        self.assertIsNone(first['fleet']['authority_error'])
        self.down=True;self.ledger.sync(P,O,True)
        failed=self.ledger.view(P,O)
        self.assertEqual(failed['fleet']['authority_error'],'fleet_authority_unavailable')
        self.assertIsNone(failed['delete_after'])



    def test_authoritative_retention_requires_fresh_claim_and_survives_restart(self):
        import retention
        def transport(body):
            if self.down:raise BillingError('fleet_authority_unavailable')
            action=body['action']
            if action.startswith('retention-'):
                return (retention.claim if action=='retention-claim' else retention.finish)(self.authority,'node',P,O,body['claim_id'])
            return dict(self.transport(body),retention=retention.status(self.authority,'node',P,O))
        self.ledger.transport=transport
        self.ledger.target=10000
        self.ledger.sync(P,O,True)
        self.charge(10000,'drain');self.ledger.sync(P,O,True)
        self.assertEqual(self.ledger.view(P,O)['delete_after'],self.now+RETENTION_SECONDS)
        self.advance(RETENTION_SECONDS+1)
        self.down=True;self.assertFalse(self.ledger.claim_expired(P,O,'vm-retention'))
        self.down=False;self.assertTrue(self.ledger.claim_expired(P,O,'vm-retention'))
        self.ledger=self.open();self.ledger.transport=transport
        self.assertTrue(self.ledger.view(P,O)['deletion_committed'])
        self.assertTrue(self.ledger.claim_expired(P,O,'vm-retention'))
        self.ledger.finish_deletion(P,O,'vm-retention')
        self.assertFalse(self.ledger.view(P,O)['deletion_committed'])
