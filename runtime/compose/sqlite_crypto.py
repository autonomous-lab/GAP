"""SQLCipher connections for worker-owned databases, with atomic plaintext migration."""
import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import stat

KEYRING_ENV = 'GAP_WORKER_DB_KEYRING'
import sqlite3


def _ring(path):
    path = Path(path)
    try:
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o077:
            raise ValueError()
        ring = json.loads(path.read_text())
        if ring['active'] not in ring['keys']:
            raise ValueError()
        for identity, key in ring['keys'].items():
            if not re.fullmatch('[A-Za-z0-9_-]{1,64}', identity) or not re.fullmatch('[0-9a-f]{64}', key):
                raise ValueError()
        return ring
    except Exception:
        raise RuntimeError('worker_database_keyring_unavailable') from None


def _key(path,purpose,keyring_path):
    ring = _ring(keyring_path)
    marker = Path(str(path) + '.key-id')
    try:
        metadata = marker.lstat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o077:
            raise ValueError()
        identity = marker.read_text().strip()
    except FileNotFoundError:
        identity = ring['active']
        try:
            fd = os.open(marker, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(fd, 'w') as file:
                file.write(identity + '\n')
                file.flush()
                os.fsync(file.fileno())
        except FileExistsError:
            return _key(path, purpose, keyring_path)
    except Exception:
        raise RuntimeError('worker_database_key_id_unavailable') from None
    if identity not in ring['keys']:
        raise RuntimeError('worker_database_key_unavailable')
    return hmac.new(bytes.fromhex(ring['keys'][identity]),
                    b'gap-worker-sqlcipher-v1\0' + purpose.encode(), hashlib.sha256).hexdigest()


def _unlock(db,key):
    db.execute("PRAGMA key = '" + key + "'")
    db.execute('SELECT count(*) FROM sqlite_schema').fetchone()


def _migrate(path,key):
    source = sqlite3.connect(path, timeout=30, isolation_level=None)
    source.execute('PRAGMA wal_checkpoint(TRUNCATE)')
    temporary = Path(str(path) + '.encrypting')
    temporary.unlink(missing_ok=True)
    try:
        source.execute('ATTACH DATABASE ? AS encrypted KEY ?', (str(temporary),key))
        source.execute("SELECT sqlcipher_export('encrypted')").fetchone()
        source.execute('DETACH DATABASE encrypted')
    finally:
        source.close()
    encrypted = sqlite3.connect(temporary, isolation_level=None)
    try:
        _unlock(encrypted, key)
    finally:
        encrypted.close()
    os.chmod(temporary, Path(path).stat().st_mode & 0o777)
    with temporary.open('rb') as file:
        os.fsync(file.fileno())
    os.replace(temporary, path)
    for suffix in ('-wal', '-shm'):
        Path(str(path) + suffix).unlink(missing_ok=True)
    directory = os.open(Path(path).parent or Path('.'), os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def connect(path,purpose,**options):
    keyring_path = os.environ.get(KEYRING_ENV)
    if not keyring_path:
        return sqlite3.connect(path, **options)
    probe = sqlite3.connect(':memory:')
    try:
        if not probe.execute('PRAGMA cipher_version').fetchone():
            raise RuntimeError('worker_sqlcipher_unavailable')
    finally:
        probe.close()
    path = Path(path)
    if path.is_symlink():
        raise RuntimeError('unsafe_worker_database_path')
    key = _key(path, purpose, keyring_path)
    if path.exists() and path.stat().st_size >= 16:
        with path.open('rb') as file:
            plaintext = file.read(16) == b'SQLite format 3\0'
        if plaintext:
            _migrate(path, key)
    db = sqlite3.connect(path, **options)
    try:
        _unlock(db, key)
    except Exception:
        db.close()
        raise
    return db


def backup(source, destination, purpose):
    """Create a transactionally consistent encrypted backup and its key marker."""
    source, destination = Path(source), Path(destination)
    marker = Path(str(destination) + '.key-id')
    if destination.exists() or marker.exists():
        raise RuntimeError('worker_database_backup_exists')
    destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    origin = target = None
    try:
        origin = connect(source, purpose, timeout=30)
        target = connect(destination, purpose, timeout=30)
        origin.backup(target, pages=256, sleep=0.01)
        if target.execute('PRAGMA integrity_check').fetchone()[0] != 'ok':
            raise RuntimeError('worker_database_backup_invalid')
        target.close()
        target = None
        os.chmod(destination, 0o600)
        with destination.open('rb') as file:
            os.fsync(file.fileno())
        directory = os.open(destination.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    except Exception:
        if target is not None:
            target.close()
        destination.unlink(missing_ok=True)
        marker.unlink(missing_ok=True)
        raise
    finally:
        if origin is not None:
            origin.close()


def prepare(root):
    if not os.environ.get(KEYRING_ENV):
        return 0
    root = Path(root).resolve()
    prepared = 0
    purposes = {'jobs.sqlite': 'jobs', 'microvm-credits.sqlite': 'microvm-credits',
                'fleet-capacity.sqlite': 'fleet-capacity',
                'before-fleet-reservations.sqlite': 'microvm-credits'}
    for path in sorted(root.rglob('*.sqlite')):
        if path.is_symlink() or not path.is_file():
            raise RuntimeError('unsafe_worker_database_path')
        purpose = purposes.get(path.name, 'worker-file:' + str(path.relative_to(root)))
        connect(path, purpose).close()
        prepared += 1
    return prepared
