#!/usr/bin/env python3
"""Select the immutable Debian base for newly created classic microVMs."""
import argparse
import json
import os
from pathlib import Path
import tempfile


def configure(root):
    root=Path(root).resolve()
    image=root/'data/gap-compose/debian-guest-image-v1'
    for asset in ('SHA256SUMS','rootfs.ext4','vmlinuz','initramfs'):
        if not (image/asset).is_file():
            raise ValueError(f'missing Debian image asset: {asset}')
    path=root/'data/gap-compose/config/runner.json'
    config=json.loads(path.read_text())
    if not isinstance(config.get('hypervisor'),dict):
        raise ValueError('managed hypervisor is not configured')
    config['hypervisor']['debian_image_dir']='/debian-images'
    descriptor,temporary=tempfile.mkstemp(prefix=path.name+'.new-',dir=path.parent)
    try:
        stat=path.stat()
        os.fchmod(descriptor,stat.st_mode & 0o777)
        os.fchown(descriptor,stat.st_uid,stat.st_gid)
        with os.fdopen(descriptor,'w') as output:
            json.dump(config,output,indent=2)
            output.write('\n')
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary,path)
    finally:
        if os.path.exists(temporary):os.unlink(temporary)


if __name__=='__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('--root',type=Path,required=True)
    args=parser.parse_args()
    configure(args.root)
    print('New classic VMs will use debian-v1 after the worker restarts')
