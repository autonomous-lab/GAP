"""Collection isolation and retained storage accounting without a hypervisor."""
import json
from pathlib import Path
from unittest.mock import patch

import unittest
from types import SimpleNamespace
import test_microvm as fixture
PROJECT, OWNER, VM = fixture.PROJECT, fixture.OWNER, fixture.VM
from microvm import VMError
from lifecycle import Runtime


class CollectionTests(unittest.TestCase):
    setUp = fixture.MicroVMTests.setUp
    def test_admin_inventory_filters_before_counting_and_pagination(self):
        from runner import Runner,Failure
        other=dict(self.meta,project_id='prj_'+'d'*24,vm_id='vm_'+'e'*32,owner_did='did:gap:'+'f'*64)
        self.manager.save(other)
        runner=Runner.__new__(Runner);runner.hypervisor=self.manager;runner.runtime=None;runner.ingress=None
        status,data=runner.rpc({'method':'GET','action':'admin/inventory','body':{'project_id':PROJECT}})
        self.assertEqual(status,200);self.assertEqual(data['total'],1)
        self.assertEqual([v['vm_id'] for v in data['vms']],[VM])
        self.assertEqual(runner.rpc({'method':'GET','action':'admin/inventory','body':{}})[1]['total'],2)
        with self.assertRaises(Failure):runner.rpc({'method':'GET','action':'admin/inventory','body':{'project_id':'invalid'}})
    def extra(self):
        self.manager.quota_provider=lambda *_:{'vcpus':4,'memory_mib':8192,'max_vms':2}
        with patch.object(self.manager,'prepare',side_effect=lambda m:self.manager.folder(m).mkdir()):
            result=self.manager.perform(PROJECT,OWNER,'vm/create',{'new_vm':True,'start':False})
        return self.manager.read(PROJECT,OWNER,result['vm']['vm_id'])

    def test_two_vms_share_project_but_not_identity_or_mutations(self):
        extra=self.extra()
        self.assertNotEqual(extra['vm_id'],VM)
        self.assertEqual(self.manager.read(PROJECT,OWNER)['vm_id'],VM)
        self.assertEqual(self.manager.vm_count(OWNER),2)
        self.assertEqual(len(self.manager.list(PROJECT,OWNER)),2)
        with self.assertRaisesRegex(VMError,'max_vms'):
            self.manager.perform(PROJECT,OWNER,'vm/create',{'new_vm':True,'start':False})
        with self.assertRaisesRegex(VMError,'project_mismatch'):
            self.manager.read('prj_'+'f'*24,OWNER,extra['vm_id'])
        with self.assertRaisesRegex(VMError,'owner_mismatch'):
            self.manager.read(PROJECT,'did:gap:'+'f'*64,extra['vm_id'])
        self.manager.perform(PROJECT,OWNER,'vm/update',{'vm_id':extra['vm_id'],'memory_mib':512})
        self.assertEqual(self.manager.read(PROJECT,OWNER,VM)['memory_mib'],1024)
        self.assertEqual(self.manager.read(PROJECT,OWNER,extra['vm_id'])['memory_mib'],512)
        self.manager.perform(PROJECT,OWNER,'vm/destroy',{'vm_id':extra['vm_id'],'delete_data':True,'confirm_data_loss':True})
        self.assertEqual(self.manager.read(PROJECT,OWNER,VM)['state'],'stopped')
        self.assertEqual(self.manager.vm_count(OWNER),1)

    def test_retained_disks_belong_to_one_generation_after_replacement(self):
        extra=self.extra()
        for meta in [self.meta,extra]:
            (self.manager.folder(meta)/'data.bin').write_bytes(b'x'*4096)
        runtime=Runtime.__new__(Runtime);runtime.manager=self.manager
        before=runtime.storage_bytes(self.meta)
        extra_before=runtime.storage_bytes(extra)
        self.manager.perform(PROJECT,OWNER,'vm/destroy',{'vm_id':VM})
        with patch.object(self.manager,'prepare',side_effect=lambda m:self.manager.folder(m).mkdir()):
            result=self.manager.perform(PROJECT,OWNER,'vm/create',{'new_vm':True,'start':False})
        fresh=self.manager.read(PROJECT,OWNER,result['vm']['vm_id'])
        retained=self.manager.read(PROJECT,OWNER,VM)
        self.assertEqual(retained['state'],'destroyed')
        self.assertEqual(retained['catalog_key'],VM)
        self.assertGreaterEqual(runtime.storage_bytes(retained),before)
        self.assertEqual(runtime.storage_bytes(extra),extra_before)
        self.assertEqual(runtime.storage_bytes(fresh),0)
        self.assertEqual(self.manager.vm_count(OWNER),2)
        self.assertEqual(len(self.manager.list(PROJECT,OWNER)),3)

    def test_recovery_finishes_archived_catalog_rename(self):
        self.manager.perform(PROJECT,OWNER,'vm/destroy',{'vm_id':VM})
        self.manager.catalog(PROJECT).replace(self.manager.catalog(VM))
        # Simulate a process exit between the atomic rename and metadata update.
        Runtime(SimpleNamespace(hypervisor=self.manager),{'state_dir':str(self.manager.root)})
        archived=self.manager.read(PROJECT,OWNER,VM)
        self.assertEqual(archived['catalog_key'],VM)
        self.assertFalse(self.manager.catalog(PROJECT).exists())
        self.assertEqual(len(self.manager.list(PROJECT,OWNER)),1)
