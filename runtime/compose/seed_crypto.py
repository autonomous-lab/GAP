"""Authenticated microVM seed storage; plaintext is held only in anonymous memory."""
from contextlib import contextmanager
import hashlib
import hmac
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from microvm import VMError

MAGIC = b'GAPSEED1'
MAX_SEED_BYTES = 8 * 1024 * 1024


class SeedCrypto:
    def __init__(self, disk_crypto):
        self.disk_crypto = disk_crypto

    def enabled(self, meta):
        return bool(meta.get('disk_encryption'))

    def cipher(self, meta, purpose):
        disk_key = self.disk_crypto.key(meta)
        if disk_key is None:
            raise VMError('seed_encryption_key_required')
        key = hmac.new(disk_key, b'gap-seed-v1\0' + purpose, hashlib.sha256).digest()
        return AESGCM(key)

    def seal(self, meta, source, target, purpose):
        source, target = Path(source), Path(target)
        if source.stat().st_size > MAX_SEED_BYTES:
            raise VMError('seed_material_too_large')
        raw = source.read_bytes()
        nonce = os.urandom(12)
        pending = target.with_name(target.name + '.next')
        fd = os.open(pending, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        try:
            with os.fdopen(fd, 'wb') as output:
                output.write(MAGIC + nonce + self.cipher(meta, purpose).encrypt(nonce, raw, MAGIC + purpose))
                output.flush()
                os.fsync(output.fileno())
            pending.replace(target)
            directory = os.open(target.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
            source.unlink()
            directory = os.open(source.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        except Exception:
            pending.unlink(missing_ok=True)
            raise

    def open_bytes(self, meta, source, purpose):
        source = Path(source)
        size = source.stat().st_size
        if size < len(MAGIC) + 12 + 16 or size > MAX_SEED_BYTES + 64:
            raise VMError('seed_material_invalid')
        payload = source.read_bytes()
        if payload[:len(MAGIC)] != MAGIC:
            raise VMError('seed_material_format_invalid')
        nonce = payload[len(MAGIC):len(MAGIC) + 12]
        try:
            return self.cipher(meta, purpose).decrypt(
                nonce, payload[len(MAGIC) + 12:], MAGIC + purpose)
        except Exception:
            raise VMError('seed_material_authentication_failed') from None

    def protect(self, meta, folder):
        if not self.enabled(meta):
            return
        folder = Path(folder)
        pairs = (
            (folder / 'seed.ext4', folder / 'seed.ext4.enc', b'image'),
            (folder / 'seed' / 'ssh_host_ed25519_key',
             folder / 'seed' / 'ssh_host_ed25519_key.enc', b'host-key'),
        )
        for clear, encrypted, purpose in pairs:
            if encrypted.exists():
                self.open_bytes(meta, encrypted, purpose)
                self._unlink(clear)
            elif clear.exists():
                self.seal(meta, clear, encrypted, purpose)
            else:
                raise VMError('seed_material_missing')

    def rebuild(self, meta, folder):
        folder = Path(folder)
        seed = folder / 'seed'
        if not self.enabled(meta):
            pending = folder / 'seed-next.ext4'
            with pending.open('wb') as image:
                image.truncate(4 * 1024 * 1024)
            self._mkfs(seed, pending)
            pending.replace(folder / 'seed.ext4')
            return

        memory_root = Path('/dev/shm')
        if not memory_root.is_dir():
            raise VMError('seed_tmpfs_unavailable')
        with tempfile.TemporaryDirectory(prefix='gap-seed-', dir=memory_root) as temporary:
            temporary = Path(temporary)
            staged_seed = temporary / 'seed'
            staged_seed.mkdir(mode=0o700)
            for name in ('authorized_keys', 'ssh_host_ed25519_key.pub', 'runtime.json'):
                shutil.copy2(seed / name, staged_seed / name)
            clear_key = seed / 'ssh_host_ed25519_key'
            if clear_key.exists():
                shutil.copy2(clear_key, staged_seed / clear_key.name)
            else:
                (staged_seed / clear_key.name).write_bytes(
                    self.open_bytes(meta, seed / 'ssh_host_ed25519_key.enc', b'host-key')
                )
            (staged_seed / clear_key.name).chmod(0o600)
            image = temporary / 'seed.ext4'
            with image.open('wb') as output:
                output.truncate(4 * 1024 * 1024)
            self._mkfs(staged_seed, image)
            self.seal(meta, image, folder / 'seed.ext4.enc', b'image')
            self._unlink(folder / 'seed.ext4')
        if clear_key.exists():
            self.seal(meta, clear_key, seed / 'ssh_host_ed25519_key.enc', b'host-key')

    @staticmethod
    def _mkfs(seed, image):
        result = subprocess.run(
            ['mkfs.ext4', '-q', '-F', '-d', str(seed), str(image)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=30,
        )
        if result.returncode:
            raise VMError('seed_image_creation_failed')

    @staticmethod
    def _unlink(path):
        path = Path(path)
        if not path.exists():
            return
        path.unlink()
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)

    @contextmanager
    def image(self, meta, folder):
        folder = Path(folder)
        if not self.enabled(meta):
            yield str(folder / 'seed.ext4'), ()
            return
        self.protect(meta, folder)
        raw = self.open_bytes(meta, folder / 'seed.ext4.enc', b'image')
        fd = os.memfd_create('gap-seed-image', os.MFD_CLOEXEC)
        try:
            offset = 0
            while offset < len(raw):
                offset += os.write(fd, raw[offset:])
            os.lseek(fd, 0, 0)
            yield f'/proc/self/fd/{fd}', (fd,)
        finally:
            os.close(fd)
