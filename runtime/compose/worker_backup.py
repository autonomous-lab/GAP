"""Create a consistent encrypted snapshot of the worker's SQLite databases."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import time

from sqlite_crypto import backup


DATABASES = (
    ('jobs/jobs.sqlite', 'jobs'),
    ('jobs/microvm-credits.sqlite', 'microvm-credits'),
    ('jobs/before-fleet-reservations.sqlite', 'microvm-credits'),
    ('vm/fleet-capacity.sqlite', 'fleet-capacity'),
)


def create(root, output):
    root, output = Path(root).resolve(), Path(output).resolve()
    backup_root = root / 'backups'
    if output.parent != backup_root or not re.fullmatch(r'[A-Za-z0-9_.-]{1,128}', output.name):
        raise RuntimeError('worker_backup_must_stay_inside_state')
    backup_root.mkdir(mode=0o700, exist_ok=True)
    if backup_root.is_symlink():
        raise RuntimeError('unsafe_worker_backup_path')
    os.chmod(backup_root, 0o700)
    pending = output.with_name(output.name + '.next')
    if output.exists() or pending.exists():
        raise RuntimeError('worker_backup_exists')
    pending.mkdir(mode=0o700)
    files = []
    try:
        for relative, purpose in DATABASES:
            source = root / relative
            if not source.exists():
                continue
            target = pending / relative
            backup(source, target, purpose)
            files.append({'path': relative, 'sha256': hashlib.sha256(target.read_bytes()).hexdigest(),
                          'key_id': Path(str(target) + '.key-id').read_text().strip()})
        manifest = pending / 'manifest.json'
        manifest.write_text(json.dumps({'format': 1, 'created_at': int(time.time()), 'files': files},
                                       sort_keys=True) + '\n')
        manifest.chmod(0o600)
        pending.replace(output)
        return files
    except Exception:
        import shutil
        shutil.rmtree(pending, ignore_errors=True)
        raise


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--root', default='/data')
    parser.add_argument('--output', required=True)
    arguments = parser.parse_args()
    print(json.dumps({'backed_up': len(create(arguments.root, arguments.output))}))
