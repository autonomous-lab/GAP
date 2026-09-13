import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest

from runner import Runner,Failure


class MigrationPolicyTests(unittest.TestCase):
    def test_origin_policy_unavailable_never_falls_back_to_local_approval(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);runner=Runner.__new__(Runner)
            runner.hypervisor=SimpleNamespace(root=root)
            runner.ingress_config=None;runner.hypervisor_config={}
            runner.path=root/'config.json'
            runner.path.write_text(json.dumps(dict(approved_only=True,node_url='http://localhost',hypervisor={})))
            origin=dict(node_id='source',owner_did='owner',migration_id='move_test')
            (root/'migration-origins.json').write_text(json.dumps({'moved':origin}))
            calls=[]
            runner.workload_policies_local=lambda projects:calls.append(projects) or {'local':{'allowed':True}}
            runner.origin_policy=lambda _:dict(policy={'owner_did':'owner','generation':7,'allowed':False},
                approval=dict(project_id='moved',owner_did='owner',managed=True,quota=dict(vcpus=4,memory_mib=8192)))
            self.assertFalse(runner.workload_policies(['moved','local'])['moved']['allowed'])
            self.assertEqual(calls,[['local']])
            self.assertTrue(runner.authorize('moved','owner')['managed'])
            def unavailable(_):raise OSError('offline')
            runner.origin_policy=unavailable
            self.assertNotIn('moved',runner.workload_policies(['moved','local']))
            self.assertEqual(calls,[['local'],['local']])
            with self.assertRaisesRegex(Failure,'compose_origin_approval_unavailable_or_revoked'):
                runner.authorize('moved','owner')
            with self.assertRaisesRegex(Failure,'migration_origin_binding_mismatch'):
                runner.authorize('moved','other-owner')


if __name__=='__main__':unittest.main()
