"""Migrate a funded isolated wallet, then exercise actual KVM reservations."""
import os
from pathlib import Path
import unittest

from fleet_kvm_test import FleetAcceptance
import integration_test
from billing import Ledger,BillingError,UNITS
from fleet_billing import FleetLedger
import wallet_migration


@unittest.skipUnless(os.environ.get('GAP_TEST_FLEET')=='1','explicit isolated fleet/KVM opt-in required')
class WalletMigrationAcceptance(FleetAcceptance):
    def configure_fleet_fixture(self,config,root,project,owner):
        self.authority_path=root/'control.sqlite'
        self.control_port=integration_test.free_port();self.control_node_token='n'*64
        self.start_controller()
        a=self.authority
        self.customer=a.create_customer('operator','customer','Isolated legacy migration')['customer_id']
        a.attach_principal('operator','agent',self.customer,'agent',owner)
        a.attach_project('operator','project',self.customer,project,'test-node',owner)
        token=root/'control-node.token';token.write_text(self.control_node_token);token.chmod(0o600)
        config['fleet_billing']=dict(url=f'http://127.0.0.1:{self.control_port}',token_file=str(token),
            operator_id='test-operator',node_id='test-node',projects=[],target_microcredits=100000,lease_seconds=15)
        Path(config['state_dir']).mkdir(mode=0o700,parents=True,exist_ok=True)
        ledger=Ledger(Path(config['state_dir'])/'microvm-credits.sqlite')
        price={'version':'fleet-kvm-test','vcpu_hour':10000,'gib_ram_hour':10000,'gb_disk_month':100000,'gb_in':10000,'gb_out':10000}
        ledger.set_pricing('enforced',price);ledger.topup(project,owner,1000000,'legacy-funding')
        with ledger.db() as db:
            ledger._charge(db,project,owner,'legacy-usage',dict.fromkeys(UNITS,0)|{'vcpu_ms':13320},'enforced',price)
        self.assertEqual(ledger.view(project,owner)['spent_microcredits'],37)

    def exercise_managed(self,request,prefix,token,other_token,runner,project,owner,approval,root):
        body=dict(project_id=project,owner_did=owner)
        prepared=runner.operator(dict(body,action='prepare-wallet-migration'))
        self.assertEqual(self.authority.wallet(self.customer)['balance_microcredits'],0)
        fields={k:prepared[k] for k in ('node_id','project_id','owner_did','transfer_id','snapshot','snapshot_digest')}
        self.control.application.handle('POST','/operator','o'*64,dict(fields,action='authorize-wallet-import',request_id='review'))
        transport=runner.runtime.ledger.transport
        def lost(body):
            result=transport(body)
            if body['action']=='wallet-import':raise BillingError('lost_commit_ack')
            return result
        runner.runtime.ledger.transport=lost
        with self.assertRaisesRegex(BillingError,'lost_commit_ack'):
            runner.operator(dict(body,action='commit-wallet-migration'))
        self.assertEqual(self.authority.wallet(self.customer)['balance_microcredits'],999963)
        config=runner.runtime.ledger.config;path=runner.runtime.ledger.path
        runner.runtime.ledger=FleetLedger(path,config)
        runner.operator(dict(body,action='commit-wallet-migration'))
        self.assertEqual(self.authority.wallet(self.customer)['balance_microcredits'],999963)
        config['projects']=[project]
        runner.runtime.ledger=FleetLedger(path,config)
        runner.hypervisor.capacity.configure(config)
        super().exercise_managed(request,prefix,token,other_token,runner,project,owner,approval,root)
        print('PASS: funded legacy wallet fence, exact import approval, lost acknowledgement, restored ledger, historical usage baseline and real KVM lifecycle',flush=True)


if __name__=='__main__':
    # Run this subclass only; importing the base must not duplicate its suite.
    unittest.main(defaultTest='WalletMigrationAcceptance')
