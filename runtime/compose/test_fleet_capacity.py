"""Fault injection at durable worker/control boundaries; no production catalogs."""
import hashlib
import concurrent.futures
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0,os.environ.get('GAP_TEST_CONTROL',str(Path(__file__).resolve().parents[1]/'control')))
from authority import Authority,Failure
from service import Application
from billing import BillingError
from fleet_capacity import Capacity
from microvm import MicroVMs,VMError

P='prj_'+'a'*24
SECOND='prj_'+'b'*24
O='did:gap:'+'a'*64

class Crash(BaseException):pass


class CapacityWorkerTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        self.a=Authority(self.root/'authority.sqlite','operator')
        self.customer=self.a.create_customer('operator','customer','Test')['customer_id']
        self.a.attach_principal('operator','owner',self.customer,'agent',O)
        for project,node in [(P,'one'),(SECOND,'two')]:
            self.a.attach_project('operator',project,self.customer,project,node,O)
        self.app=Application(self.a,'admin',{'one':'one-token','two':'two-token'},allow_capacity=True)
        self.down=False;self.fault=None;self.calls=[]
        self.manager=self.make_manager('one',P)

    def make_manager(self,node,project):
        root=self.root/node;root.mkdir()
        images=root/'images';images.mkdir();manifest=[]
        for name in ('vmlinuz','initramfs','rootfs.ext4'):
            (images/name).write_bytes(name.encode());manifest.append(hashlib.sha256(name.encode()).hexdigest()+'  '+name)
        (images/'SHA256SUMS').write_text('\n'.join(manifest)+'\n')
        manager=MicroVMs(dict(state_dir=str(root/'vms'),image_dir=str(images)),None)
        manager.quota_provider=lambda *_:dict(vcpus=100,memory_mib=100000,max_vms=10)
        manager.capacity.configure(dict(projects=[project],node_id=node,operator_id='operator'),lambda b:self.transport(node,b))
        def prepare(meta):
            manager.folder(meta).mkdir();(manager.folder(meta)/'disk.qcow2').write_text('disk')
            meta['image_version']=manager.image_version()
        def start(meta):
            if not manager.execution_allowed(meta):raise VMError('capacity_did_not_allow_execution')
            meta['state']='running';manager.save(meta)
        manager.prepare=prepare;manager.start=start
        manager.alive=lambda m:m['state']=='running'
        manager.qmp=lambda m,*_:dict(status=m['state'])
        return manager

    def transport(self,node,body):
        self.calls.append((node,dict(body)))
        if self.down:raise BillingError('fleet_authority_unavailable')
        if self.fault==('before',body['action']):self.fault=None;raise Crash()
        try:r=self.app.handle('POST','/node',node+'-token',body)
        except Failure as e:raise BillingError(e.code) from None
        if self.fault==('after',body['action']):self.fault=None;raise Crash()
        return r

    def restart(self, manager=None):
        manager=manager or self.manager
        config=manager.capacity.config
        manager.capacity=Capacity(manager)
        manager.capacity.configure(config,lambda b:self.transport(config['node_id'],b))
        return manager.capacity

    def recover(self,manager=None,project=P):
        manager=manager or self.manager
        with manager.owner_lock(O):manager.capacity.reconcile_project(project,O)

    def create(self,manager=None,project=P,**body):
        manager=manager or self.manager
        return manager.perform(project,O,'vm/create',dict(start=False,**body))['vm']

    def limits(self,count=2,cpu=8,ram=2048):
        self.a.set_quotas('operator','limits',self.customer,dict(max_vms=count,cpu_quarters=cpu,memory_mib=ram),0)

    def test_shared_last_place_is_enforced_by_both_real_worker_managers(self):
        other=self.make_manager('two',SECOND)
        self.create()
        with self.assertRaisesRegex(VMError,'customer_quota_exceeded_max_vms'):self.create(other,SECOND)
        self.assertEqual(other.list(SECOND,O),[])
        self.assertEqual(other.capacity.records()[0]['phase'],'closed')
        self.assertEqual(self.a.quotas(self.customer)['allocated']['max_vms'],1)

    def test_concurrent_workers_cannot_both_create_the_last_vm(self):
        other=self.make_manager('two',SECOND)
        def create(pair):
            try:self.create(*pair);return 'created'
            except VMError as e:return str(e)
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            outcomes=list(pool.map(create,[(self.manager,P),(other,SECOND)]))
        self.assertCountEqual(outcomes,['created','customer_quota_exceeded_max_vms'])
        self.assertEqual(sum(len(m.list(p,O)) for m,p in [(self.manager,P),(other,SECOND)]),1)

    def test_crashes_after_each_durable_creation_phase_do_not_duplicate_a_vm(self):
        for phase in ('preparing','applying','finishing','active'):
            with self.subTest(phase=phase):
                self.setUp();put=self.manager.capacity.put
                def interrupted(record):
                    put(record)
                    if record['phase']==phase:raise Crash()
                with patch.object(self.manager.capacity,'put',side_effect=interrupted):
                    with self.assertRaises(Crash):self.create()
                self.restart();self.recover()
                allocated=self.a.quotas(self.customer)['allocated']['max_vms']
                self.assertEqual(allocated,len(self.manager.list(P,O)))
                self.assertLessEqual(allocated,1)

    def test_funded_execution_still_requires_confirmed_capacity(self):
        vm=self.create();meta=self.manager.read(P,O)
        self.assertTrue(self.manager.execution_allowed(meta))
        self.restart()
        self.assertFalse(self.manager.execution_allowed(meta))
        self.recover();self.assertTrue(self.manager.execution_allowed(meta))
        self.manager.capacity.configure(None)
        self.assertFalse(self.manager.execution_allowed(meta))
        with self.assertRaisesRegex(VMError,'configuration_required'):
            self.manager.perform(P,O,'vm/start',dict(vm_id=vm['vm_id']))

    def test_unmanaged_existing_catalog_cannot_be_silently_adopted(self):
        self.manager.capacity.configure(None);self.create()
        self.manager.capacity.configure(dict(projects=[P],operator_id='operator',node_id='one'),lambda b:self.transport('one',b))
        with self.assertRaisesRegex(VMError,'legacy_vm_capacity_migration_required'):
            self.manager.perform(P,O,'vm/start',dict(vm_id=self.manager.read(P,O)['vm_id']))
        self.assertEqual(self.a.quotas(self.customer)['allocated']['max_vms'],0)

    def test_create_crash_before_or_after_prepare_fences_delayed_request(self):
        for when in ('before','after'):
            with self.subTest(when=when):
                self.setUp();self.fault=(when,'capacity-prepare')
                with self.assertRaises(Crash):self.create()
                delayed=next(b for _,b in self.calls if b['action']=='capacity-prepare')
                self.restart();self.recover()
                record=self.manager.capacity.records()[0]
                self.assertEqual(record['phase'],'closed')
                # The response can be cached; only current state authorizes work.
                try:self.transport('one',delayed)
                except BillingError as e:self.assertEqual(str(e),'capacity_released')
                self.assertEqual(self.a.capacity_get('one',P,record['vm'])['state'],'released')
                self.assertEqual(self.a.quotas(self.customer)['allocated']['max_vms'],0)
                self.assertEqual(self.manager.list(P,O),[])
                self.create()

    def test_catalog_write_crashes_keep_the_same_generation_and_its_count(self):
        for state in ('creating','stopped'):
            with self.subTest(state=state):
                self.setUp();save=self.manager.save
                def interrupted(meta):
                    save(meta)
                    if meta['state']==state:raise Crash()
                with patch.object(self.manager,'save',side_effect=interrupted):
                    with self.assertRaises(Crash):self.create()
                vm=self.manager.read(P,O)['vm_id']
                self.restart();self.recover()
                self.assertEqual(self.manager.read(P,O)['vm_id'],vm)
                self.assertEqual(self.a.capacity_get('one',P,vm)['state'],'active')
                self.assertEqual(self.a.quotas(self.customer)['allocated']['max_vms'],1)

    def test_lost_commit_ack_is_replayed_without_recreating_disk(self):
        self.fault=('after','capacity-finish')
        with self.assertRaises(Crash):self.create()
        meta=self.manager.read(P,O);disk=self.manager.folder(meta)/'disk.qcow2';disk.write_text('preserved')
        self.restart();self.recover()
        self.assertEqual(disk.read_text(),'preserved')
        self.assertEqual(self.a.capacity_get('one',P,meta['vm_id'])['state'],'active')
        self.assertEqual(self.a.quotas(self.customer)['allocated']['max_vms'],1)

    def test_resize_delayed_prepare_can_never_reopen_an_aborted_transition(self):
        for when in ('before','after'):
            with self.subTest(when=when):
                self.setUp();self.limits();vm=self.create()
                self.fault=(when,'capacity-prepare')
                with self.assertRaises(Crash):self.manager.perform(P,O,'vm/update',dict(vm_id=vm['vm_id'],vcpus=2))
                delayed=[b for _,b in self.calls if b['action']=='capacity-prepare'][-1]
                self.restart();self.recover()
                try:self.transport('one',delayed)
                except BillingError as e:self.assertEqual(str(e),'capacity_revision_conflict')
                current=self.a.capacity_get('one',P,vm['vm_id'])
                self.assertEqual(current['state'],'active');self.assertEqual(current['committed']['cpu_quarters'],4)
                self.assertTrue(self.manager.execution_allowed(self.manager.read(P,O)))

    def test_resize_catalog_ack_loss_commits_new_resources_exactly_once(self):
        self.limits();vm=self.create();save=self.manager.save
        def interrupted(meta):
            save(meta)
            if meta['vcpus']==2:raise Crash()
        with patch.object(self.manager,'save',side_effect=interrupted):
            with self.assertRaises(Crash):self.manager.perform(P,O,'vm/update',dict(vm_id=vm['vm_id'],vcpus=2))
        self.assertEqual(self.a.quotas(self.customer)['allocated']['cpu_quarters'],8)
        self.restart();self.recover()
        self.assertEqual(self.a.capacity_get('one',P,vm['vm_id'])['committed']['cpu_quarters'],8)
        self.assertTrue(self.manager.execution_allowed(self.manager.read(P,O)))

    def test_failed_resize_quota_does_not_wedge_the_existing_vm(self):
        vm=self.create()
        with self.assertRaisesRegex(VMError,'customer_quota_exceeded_cpu_quarters'):
            self.manager.perform(P,O,'vm/update',dict(vm_id=vm['vm_id'],vcpus=2))
        self.assertTrue(self.manager.execution_allowed(self.manager.read(P,O)))
        self.assertEqual(self.a.quotas(self.customer)['allocated']['cpu_quarters'],4)

    def test_destruction_crashes_complete_only_the_recorded_generation_and_retention_choice(self):
        for delete in (False,True):
            with self.subTest(delete=delete):
                self.setUp();vm=self.create();save=self.manager.save
                def interrupted(meta):
                    if meta['state']=='destroyed':raise Crash()  # disk move/removal occurred; catalog still stopped
                    save(meta)
                body=dict(vm_id=vm['vm_id'],delete_data=delete,confirm_data_loss=delete)
                with patch.object(self.manager,'save',side_effect=interrupted):
                    with self.assertRaises(Crash):self.manager.perform(P,O,'vm/destroy',body)
                self.assertEqual(self.a.quotas(self.customer)['allocated']['max_vms'],1)
                self.restart();self.recover()
                self.assertEqual(self.a.quotas(self.customer)['allocated']['max_vms'],0)
                self.assertEqual((self.manager.root/'retained'/vm['vm_id']).exists(),not delete)
                self.assertEqual(self.manager.read(P,O)['state'],'destroyed')

    def test_lost_release_ack_and_partition_hold_quota_until_reconciled(self):
        vm=self.create();self.fault=('after','capacity-finish')
        with self.assertRaises(Crash):self.manager.perform(P,O,'vm/destroy',dict(vm_id=vm['vm_id']))
        self.restart();self.down=True
        with self.assertRaises(VMError):self.recover()
        self.assertFalse(self.manager.execution_allowed(self.manager.read(P,O)))
        self.down=False;self.recover()
        self.assertEqual(self.a.quotas(self.customer)['allocated']['max_vms'],0)
        self.create()

    def test_changed_authority_cannot_rebind_a_persisted_project(self):
        self.create()
        with self.assertRaisesRegex(VMError,'binding_mismatch'):
            self.manager.capacity.configure(dict(projects=[P],operator_id='other',node_id='one'),lambda b:None)


if __name__=='__main__':unittest.main()
