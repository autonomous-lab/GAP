"""Destructive test ONLY for fresh generated test VM identities, never user VMs.

Explicit opt-in required; tests real KVM, guest Docker and lifecycle persistence.
"""
import base64
import json
import os
from pathlib import Path
import time
import urllib.request
import uuid

from microvm import MicroVMs, VMError
from runner import execute_guest


def main():
    if os.environ.get('GAP_VM_TEST_ALLOW_CREATE') != '1':
        raise SystemExit('set GAP_VM_TEST_ALLOW_CREATE=1 for disposable VM lifecycle test')
    manager = MicroVMs({'state_dir': os.environ['GAP_VM_TEST_STATE_DIR'],
                       'image_dir': os.environ['GAP_VM_TEST_IMAGE_DIR'], 'diagnostic_serial': True}, execute_guest)
    project, owner = 'prj_' + uuid.uuid4().hex[:24], 'did:gap:' + uuid.uuid4().hex * 2
    def call(action, **body):
        body['request_id'] = uuid.uuid4().hex
        result = manager.perform(project, owner, 'vm/' + action, body)
        print(json.dumps({'step': action, 'result': result}), flush=True)
        return result['vm']
    def ready():
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            try:
                response = execute_guest(manager.guest(project, owner), {'action': 'vm_probe', 'body': {}}, timeout=12)
                if response.get('ok'):
                    print(json.dumps({'step': 'guest_docker_ready', 'result': response}), flush=True)
                    return
            except Exception:
                pass
            time.sleep(1)
        meta = manager.read(project, owner)
        serial = manager.folder(meta) / 'serial.log'
        if serial.exists():
            print(serial.read_text(errors='replace')[-12000:], flush=True)
        raise AssertionError('guest Docker did not become ready')
    try:
        vm = call('create', vcpus=1, memory_mib=1024, disk_gib=4, ports=[8000])
        vm_id = vm['vm_id']
        ready()
        source = '''services:
  web:
    build: .
    ports: ["8000:8000"]
    volumes: ["appdata:/persist"]
volumes:
  appdata: {}
'''
        dockerfile = '''FROM alpine:3.23
RUN apk add --no-cache busybox-extras && mkdir /www && echo managed-by-gap > /www/index.html
CMD ["sh", "-c", "echo persistent > /persist/test; busybox-extras httpd -f -p 8000 -h /www"]
'''
        body = {'request_id': uuid.uuid4().hex, 'compose_file': 'compose.yaml', 'files': {
            'compose.yaml': base64.b64encode(source.encode()).decode(),
            'Dockerfile': base64.b64encode(dockerfile.encode()).decode()}}
        result = execute_guest(manager.guest(project, owner), {'action': 'releases', 'body': body})
        print(json.dumps({'step': 'compose_build_deploy', 'result': result}), flush=True)
        assert result['ok'], result
        url = 'http://127.0.0.1:' + str(vm['ports'][0]['worker_port'])
        with urllib.request.urlopen(url, timeout=10) as response:
            assert response.read().strip() == b'managed-by-gap'
        try:
            call('update', vm_id=vm_id, memory_mib=1280)
            raise AssertionError('live resize should fail')
        except VMError as error:
            assert str(error) == 'stop_vm_before_reconfiguration'
        call('stop', vm_id=vm_id)
        call('update', vm_id=vm_id, vcpus=2, memory_mib=1280, disk_gib=5)
        # New controller instance must recover the same guest/disks/keys.
        manager = MicroVMs({'state_dir': os.environ['GAP_VM_TEST_STATE_DIR'],
                           'image_dir': os.environ['GAP_VM_TEST_IMAGE_DIR'], 'diagnostic_serial': True}, execute_guest)
        call('start', vm_id=vm_id)
        ready()
        started = execute_guest(manager.guest(project, owner), {'action': 'start', 'body': {'request_id': uuid.uuid4().hex}})
        assert started['ok'], started
        with urllib.request.urlopen(url, timeout=10) as response:
            assert response.read().strip() == b'managed-by-gap'
        print('REAL KVM + COMPOSE + STOP + RESIZE + CONTROLLER RECOVERY + RESTART: OK', flush=True)
    except VMError as error:
        print(getattr(error, 'diagnostic', str(error)), flush=True)
        raise
    finally:
        meta = manager.read(project, owner)
        if meta and meta['state'] != 'destroyed':
            if manager.alive(meta):
                call('stop', vm_id=meta['vm_id'], force=True)
            call('destroy', vm_id=meta['vm_id'], delete_data=True, confirm_data_loss=True)
            assert not manager.folder(meta).exists()
            print('TEST VM AND ITS DISKS DELETED', flush=True)


if __name__ == '__main__':
    main()
