import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from disk_crypto import DiskCrypto
from microvm import VMError
from seed_crypto import MAGIC, SeedCrypto


class SeedEncryptionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        keyring = self.root / 'keys.json'
        keyring.write_text(json.dumps({'active': 'v1', 'keys': {'v1': 'ab' * 32}}))
        keyring.chmod(0o600)
        self.disk = DiskCrypto(keyring)
        self.crypto = SeedCrypto(self.disk)
        self.meta = {'vm_id': 'vm_one', 'project_id': 'prj_one', 'owner_did': 'did:gap:owner'}
        self.disk.initialize(self.meta)
        (self.root / 'seed').mkdir()
        (self.root / 'seed.ext4').write_bytes(b'filesystem secret')
        (self.root / 'seed' / 'ssh_host_ed25519_key').write_bytes(b'private key')

    def test_protect_removes_both_plaintext_copies_and_opens_from_memfd(self):
        self.crypto.protect(self.meta, self.root)
        self.assertFalse((self.root / 'seed.ext4').exists())
        self.assertFalse((self.root / 'seed' / 'ssh_host_ed25519_key').exists())
        for path in (self.root / 'seed.ext4.enc', self.root / 'seed' / 'ssh_host_ed25519_key.enc'):
            self.assertEqual(path.read_bytes()[:len(MAGIC)], MAGIC)
            self.assertEqual(path.stat().st_mode & 0o077, 0)
        with self.crypto.image(self.meta, self.root) as (path, fds):
            self.assertEqual(os.read(fds[0], 128), b'filesystem secret')
            self.assertEqual(path, f'/proc/self/fd/{fds[0]}')
        with self.assertRaises(OSError):
            os.fstat(fds[0])

    def test_wrong_vm_binding_and_tampering_fail(self):
        self.crypto.protect(self.meta, self.root)
        wrong = dict(self.meta, vm_id='vm_two')
        with self.assertRaisesRegex(VMError, 'authentication'):
            self.crypto.open_bytes(wrong, self.root / 'seed.ext4.enc', b'image')
        path = self.root / 'seed.ext4.enc'
        payload = bytearray(path.read_bytes())
        payload[-1] ^= 1
        path.write_bytes(payload)
        with self.assertRaisesRegex(VMError, 'authentication'):
            self.crypto.open_bytes(self.meta, path, b'image')

    def test_rebuild_uses_decrypted_host_key_only_in_ephemeral_staging(self):
        seed = self.root / 'seed'
        (seed / 'authorized_keys').write_text('client')
        (seed / 'ssh_host_ed25519_key.pub').write_text('public')
        (seed / 'runtime.json').write_text('{}')

        def fake_mkfs(staged, image):
            image.write_bytes((staged / 'ssh_host_ed25519_key').read_bytes())

        with patch.object(SeedCrypto, '_mkfs', side_effect=fake_mkfs):
            self.crypto.rebuild(self.meta, self.root)
            self.assertFalse((seed / 'ssh_host_ed25519_key').exists())
            self.assertFalse((self.root / 'seed.ext4').exists())
            self.assertEqual(
                self.crypto.open_bytes(self.meta, self.root / 'seed.ext4.enc', b'image'),
                b'private key',
            )
            (seed / 'runtime.json').write_text('{"updated":true}')
            self.crypto.rebuild(self.meta, self.root)
            self.assertFalse((seed / 'ssh_host_ed25519_key').exists())


if __name__ == '__main__':
    unittest.main()
