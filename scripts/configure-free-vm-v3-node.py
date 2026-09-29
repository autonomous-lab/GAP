#!/usr/bin/env python3
"""Select the immutable, direct-egress image for future anonymous VMs."""
import argparse
import json
import os
from pathlib import Path
import tempfile


def configure(root):
    root=Path(root).resolve()
    image=root/'data/gap-compose/free-guest-image-v3'
    for asset in ('SHA256SUMS','rootfs.ext4','vmlinuz','initramfs'):
        if not (image/asset).is_file():
            raise ValueError(f'missing free-vm-v3 image asset: {asset}')
    path=root/'data/gap-compose/config/runner.json'
    config=json.loads(path.read_text())
    if not isinstance(config.get('hypervisor'),dict):
        raise ValueError('managed hypervisor is not configured')
    if not config['hypervisor'].get('cpu_quota_socket'):
        raise ValueError('host CPU/egress broker is required')
    config['hypervisor']['free_image_v3_dir']='/free-images-v3'
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
    print('New anonymous VMs will use free-vm-v3 after the worker restarts')
