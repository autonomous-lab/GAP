"""One-VM offline conversion to the current encrypted qcow2 KDF policy."""
import argparse
import json
import os
from pathlib import Path

from disk_crypto import DiskCrypto


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--catalog', type=Path, required=True)
    parser.add_argument('--vm-folder', type=Path, required=True)
    parser.add_argument('--keyring', type=Path, required=True)
    parser.add_argument('--confirm-vm', required=True)
    args = parser.parse_args()
    meta = json.loads(args.catalog.read_text())
    if meta['vm_id'] != args.confirm_vm or args.vm_folder.name != args.confirm_vm:
        raise RuntimeError('VM identity mismatch')
    if meta['state'] != 'stopped':
        raise RuntimeError('VM must be stopped before disk conversion')
    if not meta.get('disk_encryption'):
        raise RuntimeError('VM disk is not encrypted')
    disk = args.vm_folder / 'disk.qcow2'
    pending = args.vm_folder / 'disk.kdf-next.qcow2'
    backup = args.vm_folder / 'disk.kdf-original.qcow2'
    if not disk.is_file() or pending.exists() or backup.exists():
        raise RuntimeError('disk source or conversion targets not in expected state')
    crypto = DiskCrypto(args.keyring)
    crypto.execute(meta, 'check', disk)
    crypto.execute(meta, 'convert', disk, output=pending)
    crypto.execute(meta, 'check', pending)
    with pending.open('rb') as converted: os.fsync(converted.fileno())
    disk.replace(backup)
    try: pending.replace(disk)
    except Exception:
        backup.replace(disk)
        raise
    directory = os.open(args.vm_folder, os.O_RDONLY | os.O_DIRECTORY)
    try: os.fsync(directory)
    finally: os.close(directory)
    print(json.dumps({'vm_id': meta['vm_id'], 'converted_disk_bytes': disk.stat().st_size,
                      'backup': str(backup), 'disk': str(disk)}))


if __name__ == '__main__': main()
