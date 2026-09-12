"""Source fencing, operator approval, lost acknowledgements and credit conservation."""
import concurrent.futures
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
import threading
from contextlib import nullcontext
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0,os.environ.get('GAP_TEST_CONTROL',str(Path(__file__).resolve().parents[1]/'control')))
from authority import Authority,Failure
from service import Application
from billing import Ledger,BillingError,UNITS
from fleet_billing import FleetLedger
from test_fleet_billing import P,O,PRICE
import wallet_migration as migration


class MigrationTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name);self.path=self.root/'ledger.sqlite';self.now=1000
        self.a=Authority(self.root/'authority.sqlite','operator',lambda:self.now)
        self.customer=self.a.create_customer('operator','customer','Test')['customer_id']
        self.a.attach_principal('operator','owner',self.customer,'agent',O)
        self.a.attach_project('operator','project',self.customer,P,'node',O)
        self.app=Application(self.a,'admin',{'node':'worker','other':'other'},allow_reservations=True)
        self.config=dict(projects=[],operator_id='operator',node_id='node',target_microcredits=100,lease_seconds=10)
        self.down=False;self.lost=False;self.calls=[]
        self.ledger=self.open()
        self.ledger.set_pricing('enforced',PRICE)
        self.ledger.topup(P,O,1000,'fund')
        with self.ledger.db() as db:
            self.ledger._charge(db,P,O,'old-use',dict.fromkeys(UNITS,0)|{'vcpu_ms':25},'enforced',PRICE)
            db.execute("UPDATE accounts SET remainder='1/2',budget=800,budget_spent=25,budget_epoch=2 WHERE project=?",(P,))

    def open(self):
        return FleetLedger(self.path,self.config,lambda:self.now,lambda:self.now,self.transport)

    def transport(self,body):
        self.calls.append(body)
        if self.down:raise BillingError('fleet_authority_unavailable')
        try:result=self.app.handle('POST','/node','worker',body)
        except Failure as e:raise BillingError(e.code) from None
        if self.lost:raise BillingError('lost_ack')
        return result

    def authorize(self,snapshot):
        body={k:snapshot[k] for k in ('node_id','project_id','owner_did','transfer_id','snapshot','snapshot_digest')}
        return self.app.handle('POST','/operator','admin',dict(body,action='authorize-wallet-import',request_id='approval'))

    def test_source_is_fenced_before_credit_and_survives_restart_configuration_removal(self):
        snapshot=migration.prepare(self.ledger,P,O)
        self.assertEqual(snapshot['snapshot']['balance'],975)
        self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'],0)
        for ledger in [self.ledger,self.open(),Ledger(self.path)]:
            self.assertFalse(ledger.lease_allowed(P))
            with self.assertRaisesRegex(BillingError,'legacy_wallet_fenced'):ledger.topup(P,O,1,'another')
            with self.assertRaisesRegex(BillingError,'legacy_wallet_fenced'):ledger.set_budget(P,O,None,'budget')
            with self.assertRaisesRegex(BillingError,'legacy_wallet_fenced'):ledger.claim_expired(P,O,'vm_old')
        self.assertEqual(migration.prepare(self.open(),P,O),snapshot)

    def test_worker_cannot_mint_money_without_exact_operator_authorization(self):
        snapshot=migration.prepare(self.ledger,P,O)
        with self.assertRaisesRegex(BillingError,'wallet_import_not_authorized'):migration.commit(self.ledger,P,O)
        self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'],0)
        body={k:snapshot[k] for k in ('node_id','project_id','owner_did','transfer_id','snapshot','snapshot_digest')}
        with self.assertRaises(Failure):self.app.handle('POST','/operator','worker',dict(body,action='authorize-wallet-import',request_id='x'))
        wrong=dict(body,snapshot_digest='f'*64)
        with self.assertRaisesRegex(Failure,'migration_digest_mismatch'):self.app.handle('POST','/operator','admin',dict(wrong,action='authorize-wallet-import',request_id='x'))
        self.authorize(snapshot)
        request={k:snapshot[k] for k in ('project_id','owner_did','transfer_id','snapshot_digest')}
        with self.assertRaises(Failure):self.app.handle('POST','/node','other',dict(request,action='wallet-import',request_id='x'))
        with self.assertRaises(Failure):self.app.handle('POST','/node','worker',dict(request,action='wallet-import',request_id='x',snapshot_digest='e'*64))

    def test_lost_ack_restart_and_repeated_commit_credit_once(self):
        snapshot=migration.prepare(self.ledger,P,O);self.authorize(snapshot)
        self.lost=True
        with self.assertRaisesRegex(BillingError,'lost_ack'):migration.commit(self.ledger,P,O)
        self.assertEqual(migration.status(self.ledger,P,O)['state'],'fenced')
        self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'],975)
        self.ledger=self.open();self.lost=False
        self.assertEqual(migration.commit(self.ledger,P,O)['state'],'committed')
        self.assertEqual(migration.commit(self.open(),P,O)['state'],'committed')
        self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'],975)
        self.assertEqual(self.a.wallet(self.customer)['spent_microcredits'],25)
        with self.ledger.db() as db:
            self.assertEqual(db.execute('SELECT balance FROM accounts').fetchone()[0],0)
            self.assertEqual(db.execute('SELECT count(*) FROM entries WHERE operation LIKE ?',('fleet-transfer:%',)).fetchone()[0],1)
        with self.assertRaisesRegex(BillingError,'fleet_configuration_required'):Ledger(self.path).view(P,O)

    def test_activation_preserves_history_carries_budget_and_settles_only_new_usage(self):
        snapshot=migration.prepare(self.ledger,P,O);self.authorize(snapshot);migration.commit(self.ledger,P,O)
        self.config['projects']=[P];self.ledger=self.open();self.ledger.sync(P,O,True)
        view=self.ledger.view(P,O)
        self.assertEqual((view['balance_microcredits'],view['spent_microcredits'],view['budget_spent_microcredits']),(100,25,25))
        with self.ledger.db() as db:
            self.assertEqual(db.execute('SELECT remainder FROM accounts').fetchone()[0],'1/2')
            self.ledger._charge(db,P,O,'new-use',dict.fromkeys(UNITS,0)|{'vcpu_ms':10},'enforced',PRICE)
        self.ledger.sync(P,O,True)
        central=self.a.wallet(self.customer)
        self.assertEqual((central['spent_microcredits'],central['total_remaining_microcredits']),(35,965))
        self.assertEqual(central['spent_microcredits']+central['total_remaining_microcredits'],1000)
        with self.assertRaisesRegex(BillingError,'use_authoritative_customer_wallet'):self.ledger.topup(P,O,1,'no')

    def test_crash_during_local_commit_rolls_back_and_retries_same_transfer(self):
        snapshot=migration.prepare(self.ledger,P,O);self.authorize(snapshot)
        with patch.object(self.ledger,'entry',side_effect=RuntimeError('crash before source commit')):
            with self.assertRaises(RuntimeError):migration.commit(self.ledger,P,O)
        with self.ledger.db() as db:
            self.assertEqual(db.execute('SELECT balance FROM accounts').fetchone()[0],975)
            self.assertEqual(db.execute('SELECT count(*) FROM fleet_bindings').fetchone()[0],0)
        self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'],975)
        migration.commit(self.open(),P,O)
        self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'],975)

    def test_concurrent_prepare_and_commit_keep_one_generation(self):
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            snapshots=list(pool.map(lambda _:migration.prepare(self.open(),P,O),range(2)))
        self.assertEqual(snapshots[0],snapshots[1]);self.authorize(snapshots[0])
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            results=list(pool.map(lambda _:migration.commit(self.open(),P,O),range(2)))
        self.assertEqual(results[0],results[1]);self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'],975)

    def test_unknown_old_usage_or_deletion_claim_prevents_fence(self):
        for sql in ['UPDATE accounts SET estimated=26','UPDATE accounts SET retention_claim=\'vm_old\'']:
            with self.ledger.db() as db:db.execute(sql)
            with self.assertRaises(BillingError):migration.prepare(self.ledger,P,O)
            with self.ledger.db() as db:
                self.assertEqual(db.execute('SELECT count(*) FROM wallet_migrations').fetchone()[0],0)
                db.execute('UPDATE accounts SET estimated=spent,retention_claim=NULL')

    def test_outage_or_changed_destination_never_unfences_source(self):
        snapshot=migration.prepare(self.ledger,P,O);self.authorize(snapshot)
        self.down=True
        with self.assertRaises(BillingError):migration.commit(self.ledger,P,O)
        self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'],0)
        self.config['node_id']='other';self.ledger=self.open()
        with self.assertRaisesRegex(BillingError,'wallet_migration_binding_conflict'):migration.commit(self.ledger,P,O)
        self.assertFalse(self.ledger.lease_allowed(P))

    def test_real_authenticated_http_transfer_and_subsequent_reservation(self):
        from service import Server
        self.app.nodes['node']='w'*64
        server=Server(('127.0.0.1',0),self.app)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        self.addCleanup(server.server_close);self.addCleanup(server.shutdown)
        token=self.root/'node.token';token.write_text('w'*64)
        self.config.update(url='http://127.0.0.1:'+str(server.server_port),token_file=str(token))
        ledger=FleetLedger(self.path,self.config,lambda:self.now)
        snapshot=migration.prepare(ledger,P,O);self.authorize(snapshot)
        self.assertEqual(migration.commit(ledger,P,O)['state'],'committed')
        self.config['projects']=[P];ledger=FleetLedger(self.path,self.config,lambda:self.now)
        ledger.sync(P,O,True)
        self.assertEqual(ledger.view(P,O)['balance_microcredits'],100)
        self.assertEqual(self.a.wallet(self.customer)['total_remaining_microcredits'],975)

    def test_operator_refuses_assets_and_persists_capacity_fence_before_money_fence(self):
        calls=[];meta={'state':'destroyed'};size=[1]
        capacity=SimpleNamespace(fence_legacy_wallet=lambda *args:calls.append('capacity'))
        runner=SimpleNamespace(authorize=lambda *args:None,
            runtime=SimpleNamespace(ledger=self.ledger,lock=lambda *_:nullcontext(),storage_bytes=lambda _:size[0]),
            hypervisor=SimpleNamespace(owner_lock=lambda *_:nullcontext(),list=lambda *_:[meta],alive=lambda _:False,capacity=capacity))
        with self.assertRaisesRegex(BillingError,'wallet_migration_requires_no_vm_assets'):
            migration.operate(runner,'prepare-wallet-migration',P,O)
        self.assertEqual(calls,[])
        size[0]=0;meta['state']='hibernated'
        with self.assertRaises(BillingError):migration.operate(runner,'prepare-wallet-migration',P,O)
        meta['state']='destroyed'
        original=migration.prepare
        def checked(*args,**kwargs):
            if not kwargs.get('dry_run'):self.assertEqual(calls,['capacity'])
            return original(*args,**kwargs)
        with patch.object(migration,'prepare',side_effect=checked):
            self.assertEqual(migration.operate(runner,'prepare-wallet-migration',P,O)['state'],'fenced')

    def test_credited_inventory_is_not_still_reported_as_pending_money(self):
        with self.ledger.db() as db:account=dict(db.execute('SELECT * FROM accounts').fetchone())
        inventory={k:account[k] for k in ('balance','spent','remainder','shadow_remainder','budget','budget_spent','budget_epoch','exhausted_at','retention_claim')}
        inventory['owner_did']=O
        self.a.stage_import('operator','inventory',self.customer,'node',P,inventory)
        self.assertEqual(self.a.wallet(self.customer)['staged_legacy_microcredits'],975)
        snapshot=migration.prepare(self.ledger,P,O);self.authorize(snapshot);migration.commit(self.ledger,P,O)
        self.assertEqual(self.a.wallet(self.customer)['staged_legacy_microcredits'],0)
        self.assertTrue(self.app.handle('GET','/health','',{})['legacy_cutover'])

    def test_capacity_fence_survives_disabled_runtime_and_missing_fleet_configuration(self):
        from fleet_capacity import Capacity
        from microvm import VMError
        root=self.root/'capacity';root.mkdir()
        manager=SimpleNamespace(root=root,list=lambda *_:[],runtime=None)
        capacity=Capacity(manager)
        capacity.fence_legacy_wallet(P,O,self.config)
        restored=Capacity(manager)
        self.assertTrue(restored.managed(P))
        with self.assertRaisesRegex(VMError,'fleet_capacity_configuration_required'):restored.require(P)
        wrong=dict(self.config,node_id='other')
        with self.assertRaisesRegex(VMError,'fleet_capacity_binding_mismatch'):restored.fence_legacy_wallet(P,O,wrong)
