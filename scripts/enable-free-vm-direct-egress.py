#!/usr/bin/env python3
"""Opt one stopped, encrypted v2 free VM into the host-filtered network path.

The guest's backing file and qcow2 are deliberately left unchanged. This is
for existing VMs whose files must survive the v3 image rollout.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import tempfile


def enable(root, project, vm):
    if not re.fullmatch(r'prj_[0-9a-f]{24}', project) or not re.fullmatch(r'vm_[0-9a-f]{32}', vm):
        raise ValueError('invalid_vm_identity')
    state=Path(root).resolve()/'data/gap-compose/worker/vm'
    catalog=state/'catalog'/f'{project}.json'
    if catalog.is_symlink():
        raise ValueError('symlink_catalog_rejected')
    meta=json.loads(catalog.read_text())
    if (meta.get('project_id')!=project or meta.get('vm_id')!=vm
            or meta.get('state')!='stopped' or meta.get('guest_image')!='free-vm-v2'
            or meta.get('direct_egress') is True or not meta.get('disk_encryption')):
        raise ValueError('vm_not_eligible_for_direct_egress')
    disk=state/'vms'/vm/'disk.qcow2'
    if disk.is_symlink() or not disk.is_file():
        raise ValueError('encrypted_disk_unavailable')
    backup_dir=state/'operator-backups'
    backup_dir.mkdir(mode=0o700,exist_ok=True)
    backup=backup_dir/f'{project}-{vm}-before-direct.json'
    if backup.exists():
        raise ValueError('backup_already_exists')
    shutil.copy2(catalog,backup)
    meta['direct_egress']=True
    descriptor,pending=tempfile.mkstemp(prefix=catalog.name+'.new-',dir=catalog.parent)
    try:
        stat=catalog.stat()
        os.fchmod(descriptor,stat.st_mode & 0o777)
        os.fchown(descriptor,stat.st_uid,stat.st_gid)
        with os.fdopen(descriptor,'w') as stream:
            json.dump(meta,stream,separators=(',',':'))
            stream.flush();os.fsync(stream.fileno())
        os.replace(pending,catalog)
    finally:
        if os.path.exists(pending):os.unlink(pending)
    return backup


if __name__=='__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('--root',required=True)
    parser.add_argument('--project',required=True)
    parser.add_argument('--vm',required=True)
    args=parser.parse_args()
    print(f'Enabled direct egress; catalog backup: {enable(args.root,args.project,args.vm)}')
