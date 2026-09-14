import json
import os
from pathlib import Path
import tempfile
import unittest


@unittest.skipUnless(Path('/usr/lib/libsqlcipher.so.0').exists(),'SQLCipher runtime required')
class SQLiteCryptoTest(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name)
        self.ring=self.root/'keys.json';self.ring.write_text(json.dumps({'active':'v1','keys':{'v1':'ab'*32,'v2':'cd'*32}}));self.ring.chmod(0o600)
        os.environ['GAP_WORKER_DB_KEYRING']=str(self.ring)
        import sqlite_crypto
        self.crypto=sqlite_crypto
    def tearDown(self):
        os.environ.pop('GAP_WORKER_DB_KEYRING',None);self.temp.cleanup()
    def test_plaintext_migration_distinct_keys_and_version_retention(self):
        path=self.root/'jobs.sqlite'
        clear=self.crypto.sqlite3.connect(path);clear.execute('CREATE TABLE values_(value TEXT)');clear.execute("INSERT INTO values_ VALUES('preserved')");clear.commit();clear.close()
        encrypted=self.crypto.connect(path,'jobs');self.assertEqual(encrypted.execute('SELECT value FROM values_').fetchone()[0],'preserved');encrypted.close()
        self.assertNotEqual(path.read_bytes()[:16],b'SQLite format 3\0')
        self.assertEqual(Path(str(path)+'.key-id').read_text().strip(),'v1')
        backup=self.root/'before-fleet-reservations.sqlite';clear=self.crypto.sqlite3.connect(backup);clear.execute('CREATE TABLE backup_(value TEXT)');clear.commit();clear.close()
        self.assertEqual(self.crypto.prepare(self.root),2)
        self.assertNotEqual(backup.read_bytes()[:16],b'SQLite format 3\0')
        from billing import Ledger
        ledger_path=self.root/'credits.sqlite';ledger=Ledger(ledger_path)
        self.assertEqual(ledger.pricing()['mode'],'shadow')
        self.assertNotEqual(ledger_path.read_bytes()[:16],b'SQLite format 3\0')
        ring=json.loads(self.ring.read_text());ring['active']='v2';self.ring.write_text(json.dumps(ring));self.ring.chmod(0o600)
        self.assertEqual(self.crypto.connect(path,'jobs').execute('SELECT count(*) FROM values_').fetchone()[0],1)
        with self.assertRaises(Exception):self.crypto.connect(path,'other-purpose')


if __name__=='__main__':unittest.main()
