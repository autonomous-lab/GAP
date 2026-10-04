#!/usr/bin/env python3
"""Select immutable swap-enabled Debian images for future GAP MicroVMs."""
import argparse
import json
import os
from pathlib import Path
import tempfile


def configure(root):
    root=Path(root).resolve()
    for folder in ('free-guest-image-v4','debian-guest-image-v2'):
        image=root/'data/gap-compose'/folder
        for asset in ('SHA256SUMS','rootfs.ext4','vmlinuz','initramfs'):
            if not (image/asset).is_file():
                raise ValueError(f'missing {folder} image asset: {asset}')
    path=root/'data/gap-compose/config/runner.json'
    config=json.loads(path.read_text())
    hypervisor=config.get('hypervisor')
    if not isinstance(hypervisor,dict) or not hypervisor.get('cpu_quota_socket'):
        raise ValueError('managed hypervisor with CPU/egress broker is required')
    hypervisor['free_image_v4_dir']='/free-images-v4'
    hypervisor['debian_image_v2_dir']='/debian-images-v2'
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
    print('New anonymous and classic MicroVMs will use swap-enabled images after the worker restarts')
