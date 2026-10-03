"""Disposable encrypted KVM hibernate/resume smoke test inside the fleet runner."""
import json
import os
from pathlib import Path
import tempfile
import time
import uuid

from microvm import MicroVMs


def main():
    if os.environ.get('GAP_FAST_FLEET_SMOKE') != '1':
        raise SystemExit('explicit smoke-test opt-in required')
    project = 'prj_' + uuid.uuid4().hex[:24]
    owner = 'did:gap:' + uuid.uuid4().hex * 2
    with tempfile.TemporaryDirectory(prefix='fs-', dir='/data') as root:
        manager = MicroVMs({
            'state_dir': root,
            'image_dir': '/images',
            'debian_image_dir': '/debian-images',
            'disk_keyring': '/config/disk-keys.json',
        }, None)
        meta = None
        try:
            result = manager.perform(project, owner, 'vm/create', {
                'request_id': uuid.uuid4().hex,
                'vcpus': 1, 'memory_mib': 1024, 'disk_gib': 8,
            })
            vm_id = result['vm']['vm_id']
            meta = manager.read(project, owner, vm_id)
            assert meta['state'] == 'running' and meta['running_fast_snapshot']
            assert manager.qemu_version(meta).startswith('QEMU emulator version 11.')
            assert manager.qmp(meta, 'query-status')['status'] == 'running'
            time.sleep(2)
            start = time.monotonic()
            manager.hibernate(meta)
            hibernate_ms = round((time.monotonic() - start) * 1000)
            assert meta['state'] == 'hibernated' and meta['snapshot_format'] == 'gapfast1'
            assert (manager.folder(meta) / 'memory.fast').exists()
            start = time.monotonic()
            manager.resume(meta)
            resume_ms = round((time.monotonic() - start) * 1000)
            assert meta['state'] == 'running'
            assert manager.qmp(meta, 'query-status')['status'] == 'running'
            print(json.dumps({'ok': True, 'vm_id': vm_id,
                              'hibernate_ms': hibernate_ms, 'resume_ms': resume_ms}), flush=True)
        finally:
            if meta is not None and manager.alive(meta):
                manager.stop(meta, True)


if __name__ == '__main__':
    main()
