import importlib.util
import json
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('compose_access', Path(__file__).resolve().parents[1] / 'scripts/compose-access.py')
access = importlib.util.module_from_spec(spec)
spec.loader.exec_module(access)
A, B = 'did:gap:' + 'a' * 64, 'did:gap:' + 'b' * 64


class AccessTests(unittest.TestCase):
    def test_old_and_new_commands_share_the_same_approval_store(self):
        scripts = Path(__file__).resolve().parents[1] / 'scripts'
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'approved.json'
            def call(name, *args):
                result = subprocess.run([sys.executable, str(scripts/name), '--file', str(path), *args],
                                        capture_output=True, text=True, check=True)
                return json.loads(result.stdout)
            self.assertTrue(call('microvm-access.py', 'grant', A)['approved'])
            self.assertEqual(call('compose-access.py', 'list')['agents'], [A])
            self.assertFalse(call('compose-access.py', 'revoke', A)['approved'])
            self.assertEqual(call('microvm-access.py', 'list')['agents'], [])

    def test_live_grant_idempotence_and_selective_revoke(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'approved.json'
            access.change(path, 'grant', A)
            access.change(path, 'grant', B)
            self.assertFalse(access.change(path, 'grant', A)['changed'])
            access.change(path, 'revoke', A)
            self.assertEqual(json.loads(path.read_text()), {'agents': [B]})
            self.assertFalse(access.change(path, 'revoke', A)['restart_required'])
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_corrupt_file_and_invalid_identity_are_not_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'approved.json'
            path.write_text('corrupt')
            with self.assertRaises(ValueError):
                access.change(path, 'grant', A)
            self.assertEqual(path.read_text(), 'corrupt')
            with self.assertRaises(ValueError):
                access.change(path, 'grant', 'all')

    def test_quotas_default_update_preserve_revoke_and_validate(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'approved.json'
            access.change(path, 'grant', A)
            self.assertEqual(access.change(path, 'list')['quotas'][A], {'vcpus': 2, 'memory_mib': 4096, 'max_vms': 1})
            access.change(path, 'set-quota', A, vcpus=4)
            access.change(path, 'grant', B, memory_mib=2048)
            self.assertEqual(access.change(path, 'list')['quotas'][A], {'vcpus': 4, 'memory_mib': 4096, 'max_vms': 1})
            self.assertFalse(access.change(path, 'grant', A)['changed'])
            before = path.read_bytes()
            for args in [('set-quota', A, 0, None), ('set-quota', A, None, None),
                         ('set-quota', 'did:gap:' + 'c'*64, 1, None), ('revoke', A, 1, None)]:
                with self.assertRaises(ValueError):
                    access.change(path, *args)
                self.assertEqual(path.read_bytes(), before)
            access.change(path, 'revoke', A)
            self.assertNotIn(A, json.loads(path.read_text())['quotas'])
            self.assertEqual(access.change(path, 'list')['quotas'][B]['memory_mib'], 2048)

    def test_vm_count_quota_can_be_raised_live_and_invalid_values_preserve_store(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'approved.json'
            access.change(path, 'grant', A)
            result = access.change(path, 'set-quota', A, max_vms=3)
            self.assertEqual(result['quota']['max_vms'], 3)
            self.assertFalse(result['restart_required'])
            self.assertEqual(access.change(path, 'list')['quotas'][A]['max_vms'], 3)
            before = path.read_bytes()
            for value in [0, -1, True, 2**31, '2']:
                with self.assertRaises(ValueError):
                    access.change(path, 'set-quota', A, max_vms=value)
                self.assertEqual(path.read_bytes(), before)

    def test_always_on_requires_approval_and_revoke_removes_permission(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'approved.json'
            access.change(path,'grant',A)
            with self.assertRaises(ValueError): access.change(path,'set-always-on',B,always_on=True)
            access.change(path,'set-always-on',A,always_on=True)
            self.assertEqual(access.change(path,'list')['always_on_agents'],[A])
            access.change(path,'revoke',A)
            self.assertEqual(access.change(path,'list')['always_on_agents'],[])

    def test_symlink_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'target'
            target.write_text('{"agents":[]}')
            path = Path(directory) / 'approved.json'
            path.symlink_to(target)
            with self.assertRaises(ValueError):
                access.change(path, 'grant', A)
            self.assertEqual(target.read_text(), '{"agents":[]}')
