import json
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from public_capacity import snapshot

class PublicCapacity(unittest.TestCase):
    def test_hibernated_vm_releases_execution_headroom_but_keeps_disk(self):
        with TemporaryDirectory() as tmp:
            root=Path(tmp);(root/'catalog').mkdir()
            for i,(state,retained) in enumerate([('hibernated',False),('destroyed',True),('destroyed',False),('migrated',False)]):
                (root/'catalog'/f'{i}.json').write_text(json.dumps(dict(state=state,retained=retained,vcpus=.5,memory_mib=256,disk_gib=4,owner_did='SECRET_OWNER',project_id='SECRET_PROJECT')))
            ledger=SimpleNamespace(pricing=lambda:dict(mode='enforced',tariff={'version':'test'}))
            with patch('admission.check',return_value={'ready':True}),patch('public_capacity.os.sched_getaffinity',return_value=set(range(4))),patch('public_capacity.shutil.disk_usage',return_value=SimpleNamespace(free=20*1024**3,total=100*1024**3)),patch('public_capacity.host_capacity.memory_mib',return_value=dict(MemTotal=16384,MemAvailable=6144,SwapTotal=16384,SwapFree=12288)):
                manager=SimpleNamespace(root=root,alive=lambda meta:meta['state']=='running')
                result=snapshot(manager,SimpleNamespace(ledger=ledger))
            self.assertTrue(result['available']);self.assertEqual(result['headroom']['vcpus'],32)
            self.assertEqual(result['headroom']['vm_slots'],32)
            self.assertEqual(result['headroom']['memory_mib'],16384)
            self.assertIn('swap_mib',result['hardware'])
            self.assertEqual(result['headroom']['disk_gib'],7)
            self.assertNotIn('SECRET',json.dumps(result))
            (root/'catalog'/'ghost.json').write_text(json.dumps(dict(state='stopped',
                retained=False,free_vm_cleanup_complete=True,vcpus=1,memory_mib=1024,disk_gib=8)))
            with patch('admission.check',return_value={'ready':True}),patch('public_capacity.os.sched_getaffinity',return_value=set(range(4))),patch('public_capacity.shutil.disk_usage',return_value=SimpleNamespace(free=20*1024**3,total=100*1024**3)),patch('public_capacity.host_capacity.memory_mib',return_value=dict(MemTotal=16384,MemAvailable=6144,SwapTotal=16384,SwapFree=12288)):
                cleaned=snapshot(manager,SimpleNamespace(ledger=ledger))
            self.assertEqual(cleaned['headroom']['disk_gib'],7)
            self.assertEqual(cleaned['headroom']['vm_slots'],32)
            (root/'catalog'/'4.json').write_text(json.dumps(dict(state='running',retained=False,vcpus=.5,memory_mib=256,disk_gib=1)))
            with patch('admission.check',return_value={'ready':True}),patch('public_capacity.os.sched_getaffinity',return_value=set(range(4))),patch('public_capacity.shutil.disk_usage',return_value=SimpleNamespace(free=20*1024**3,total=100*1024**3)),patch('public_capacity.host_capacity.memory_mib',return_value=dict(MemTotal=16384,MemAvailable=6144,SwapTotal=16384,SwapFree=12288)):
                running=snapshot(manager,SimpleNamespace(ledger=ledger))
            self.assertEqual(running['headroom']['vcpus'],31.5)
            self.assertEqual(running['headroom']['vm_slots'],31)
            (root/'catalog'/'invalid.json').write_text('{')
            self.assertFalse(snapshot(manager,SimpleNamespace(ledger=ledger))['available'])
    def test_missing_worker_is_unavailable(self):
        self.assertFalse(snapshot(None,None)['available'])
if __name__=='__main__':unittest.main()
