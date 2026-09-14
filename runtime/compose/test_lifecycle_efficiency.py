import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import Mock, patch

import billing
from billing import Ledger
from fleet_billing import FleetLedger
from lifecycle import Runtime


class LifecycleEfficiencyTests(unittest.TestCase):
    def meta(self, state):
        return dict(vm_id='vm_'+'a'*32, project_id='prj_'+'b'*24,
                    owner_did='did:gap:'+'c'*64, state=state)

    def runtime(self, root):
        runtime=Runtime.__new__(Runtime)
        runtime.states={};runtime.closed=False;runtime.fleet_renewed_at={}
        runtime.manager=SimpleNamespace(root=root, network=None)
        runtime.storage_bytes=Mock(return_value=0)
        return runtime

    def test_destroyed_empty_vm_checks_remote_deletion_at_most_once_per_minute(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);path=root/'vm.json';path.write_text(json.dumps(self.meta('destroyed')))
            runtime=self.runtime(root)
            runtime.ledger=SimpleNamespace(view=Mock(return_value={'deletion_committed':False}))
            runtime.expire=Mock()
            with patch('lifecycle.time.monotonic',side_effect=[100,100,120,120,160,160]):
                runtime.tick_project(path);runtime.tick_project(path);runtime.tick_project(path)
            self.assertEqual(runtime.ledger.view.call_count,2)
            runtime.expire.assert_not_called()

    def test_fleet_meter_skips_destroyed_empty_vm(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);catalog=root/'catalog';catalog.mkdir()
            (catalog/'destroyed.json').write_text(json.dumps(self.meta('destroyed')))
            running=dict(self.meta('running'),vm_id='vm_'+'d'*32)
            (catalog/'running.json').write_text(json.dumps(running))
            runtime=self.runtime(root)
            runtime.ledger=SimpleNamespace(fleet_allows=Mock(return_value=True),sync=Mock())
            runtime.sample=Mock()
            def stop(_):runtime.closed=True
            with patch('lifecycle.time.sleep',side_effect=stop):runtime.fleet_meter_loop()
            runtime.sample.assert_called_once_with(running,force=False)

    def test_fleet_lease_renewal_is_independent_from_minute_metering(self):
        runtime=self.runtime(Path('/unused'))
        runtime.ledger=SimpleNamespace(sync=Mock())
        meta=self.meta('running')
        with patch('lifecycle.time.monotonic',side_effect=[100,110,115]):
            runtime.renew_fleet_lease(meta)
            runtime.renew_fleet_lease(meta)
            runtime.renew_fleet_lease(meta)
        self.assertEqual(runtime.ledger.sync.call_count,2)

    def test_destroyed_storage_scan_is_cached_for_one_minute(self):
        runtime=self.runtime(Path('/unused'))
        meta=self.meta('destroyed')
        runtime.storage_bytes=Mock(return_value=0)
        with patch('lifecycle.time.monotonic',side_effect=[100,120,160]):
            self.assertEqual(runtime.metered_storage_bytes(meta),0)
            self.assertEqual(runtime.metered_storage_bytes(meta),0)
            self.assertEqual(runtime.metered_storage_bytes(meta),0)
        self.assertEqual(runtime.storage_bytes.call_count,2)

    def test_tick_billing_view_is_cached_for_fifteen_seconds(self):
        runtime=Runtime.__new__(Runtime);runtime.states={}
        meta=self.meta('running')
        view={'execution_allowed':True}
        runtime.ledger=SimpleNamespace(view=Mock(return_value=view))
        with patch('lifecycle.time.monotonic',side_effect=[100,110,115]):
            self.assertIs(runtime.billing_view(meta),view)
            self.assertIs(runtime.billing_view(meta),view)
            self.assertIs(runtime.billing_view(meta),view)
        self.assertEqual(runtime.ledger.view.call_count,2)

    def test_fleet_samples_batch_authority_checkpoints(self):
        ledger=FleetLedger.__new__(FleetLedger);ledger.sync=Mock()
        meta={'project_id':'prj_'+'b'*24,'owner_did':'did:gap:'+'c'*64}
        with patch.object(Ledger,'sample') as local_sample:
            FleetLedger.sample(ledger,meta,1,False,0,0,0,'incarnation')
        local_sample.assert_called_once_with(meta,1,False,0,0,0,'incarnation')
        ledger.sync.assert_called_once_with(meta['project_id'],meta['owner_did'],force=False)

    def test_capacity_reconciliation_is_bounded_to_five_seconds(self):
        runtime=Runtime.__new__(Runtime)
        capacity=SimpleNamespace(records=Mock(return_value=[]),projects=set())
        runtime.manager=SimpleNamespace(capacity=capacity)
        with patch('lifecycle.time.monotonic',side_effect=[100,101,105]):
            self.assertEqual(runtime.reconcile_capacity(),[])
            self.assertEqual(runtime.reconcile_capacity(),[])
            self.assertEqual(runtime.reconcile_capacity(),[])
        self.assertEqual(capacity.records.call_count,2)

    def test_ledger_reuses_connection_on_same_thread(self):
        with tempfile.TemporaryDirectory() as directory, patch('billing.connect',wraps=billing.connect) as connect:
            ledger=Ledger(Path(directory)/'credits.sqlite')
            ledger.pricing();ledger.pricing()
            self.assertEqual(connect.call_count,1)


if __name__=='__main__':unittest.main()
