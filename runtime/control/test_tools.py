import importlib.util
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest

ROOT = Path(os.environ.get('GAP_TEST_REPO', Path(__file__).resolve().parents[2]))
spec = importlib.util.spec_from_file_location('fleet_control_cli', ROOT / 'scripts/fleet-control.py')
cli = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cli)


class ToolTests(unittest.TestCase):
    def test_legacy_export_does_not_mutate_source_or_round_carry(self):
        with tempfile.TemporaryDirectory() as temp:
            source = Path(temp) / 'source with spaces.sqlite'
            with sqlite3.connect(source) as db:
                db.execute('CREATE TABLE accounts(project TEXT,owner TEXT,balance INTEGER,spent INTEGER,remainder TEXT,shadow_remainder TEXT,budget INTEGER,budget_spent INTEGER,budget_epoch INTEGER,exhausted_at REAL,retention_claim TEXT)')
                db.execute('INSERT INTO accounts VALUES(?,?,?,?,?,?,?,?,?,?,?)', ('prj_' + 'a' * 24, 'did:gap:' + 'a' * 64,
                           999999999999999, 1, '723/730', '17/730', 100, 1, 7, 123.5, 'vm_' + 'b' * 32))
            original = source.read_bytes()
            result = cli.export_legacy(source, 'node-one')
            self.assertEqual(source.read_bytes(), original)
            self.assertFalse(result['source_fenced'])
            self.assertFalse(result['spendable'])
            snapshot = result['accounts'][0]['snapshot']
            self.assertEqual(snapshot['balance'], 999999999999999)
            self.assertEqual(snapshot['remainder'], '723/730')
            self.assertEqual(snapshot['exhausted_at'], 123.5)
            self.assertEqual(snapshot['retention_claim'], 'vm_' + 'b' * 32)

    def test_private_output_refuses_clobber_and_symlink(self):
        with tempfile.TemporaryDirectory() as temp:
            target = Path(temp) / 'private.json'
            cli.write_private(target, {'token': 'test-only'})
            self.assertEqual(target.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError):
                cli.write_private(target, {'token': 'replacement'})
            link = Path(temp) / 'link.json'
            link.symlink_to(target)
            with self.assertRaises(OSError):
                cli.write_private(link, {})
            self.assertIn('test-only', target.read_text())

    def test_sqlite_online_backup_restores_registry_and_wallet(self):
        from authority import Authority
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'authority.sqlite'
            authority = Authority(path, 'operator')
            customer = authority.create_customer('operator', 'new', 'Customer')['customer_id']
            original = authority.topup('operator', 'credit', customer, 42, 'promotional')
            backup = Path(temp) / 'backup.sqlite'
            with sqlite3.connect(path) as source, sqlite3.connect(backup) as destination:
                source.backup(destination)
            restored = Authority(backup, 'operator')
            self.assertEqual(restored.wallet(customer)['balance_microcredits'], 42)
            self.assertEqual(restored.topup('operator', 'credit', customer, 42, 'promotional'), original)
            self.assertEqual(restored.wallet(customer)['balance_microcredits'], 42)


if __name__ == '__main__':
    unittest.main()
