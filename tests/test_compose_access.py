import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('compose_access', Path(__file__).resolve().parents[1] / 'scripts/compose-access.py')
access = importlib.util.module_from_spec(spec)
spec.loader.exec_module(access)
A, B = 'did:gap:' + 'a' * 64, 'did:gap:' + 'b' * 64


class AccessTests(unittest.TestCase):
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

    def test_symlink_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'target'
            target.write_text('{"agents":[]}')
            path = Path(directory) / 'approved.json'
            path.symlink_to(target)
            with self.assertRaises(ValueError):
                access.change(path, 'grant', A)
            self.assertEqual(target.read_text(), '{"agents":[]}')
