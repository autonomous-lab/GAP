import json
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from public_capacity import snapshot

class PublicCapacity(unittest.TestCase):
    def test_retained_commitments_and_no_tenant_data(self):
        with TemporaryDirectory() as tmp:
            root=Path(tmp);(root/'catalog').mkdir()
            for i,(state,retained) in enumerate([('hibernated',False),('destroyed',True),('destroyed',False),('migrated',False)]):
                (root/'catalog'/f'{i}.json').write_text(json.dumps(dict(state=state,retained=retained,vcpus=.5,memory_mib=256,disk_gib=4,owner_did='SECRET_OWNER',project_id='SECRET_PROJECT')))
            ledger=SimpleNamespace(pricing=lambda:dict(mode='enforced',tariff={'version':'test'}))
            with patch('admission.check',return_value={'ready':True}),patch('public_capacity.os.sched_getaffinity',return_value=set(range(4))),patch('public_capacity.shutil.disk_usage',return_value=SimpleNamespace(free=20*1024**3,total=100*1024**3)):
                result=snapshot(SimpleNamespace(root=root),SimpleNamespace(ledger=ledger))
            self.assertTrue(result['available']);self.assertEqual(result['headroom']['vcpus'],2.5)
            self.assertEqual(result['headroom']['disk_gib'],7)
            self.assertNotIn('SECRET',json.dumps(result))
            (root/'catalog'/'invalid.json').write_text('{')
            self.assertFalse(snapshot(SimpleNamespace(root=root),SimpleNamespace(ledger=ledger))['available'])
    def test_missing_worker_is_unavailable(self):
        self.assertFalse(snapshot(None,None)['available'])
if __name__=='__main__':unittest.main()
