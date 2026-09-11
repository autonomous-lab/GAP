"""Host-side CPU quota broker. Only descendants of the configured runner qualify.

Run as root on the host, never inside a privileged worker container. The Unix
socket carries no commands, paths or arbitrary PIDs in the host namespace.
"""
import argparse
import json
import math
import os
from pathlib import Path
import re
import socket
import socketserver
import struct
import subprocess


def quarters(value):
    if type(value) not in (int, float) or not math.isfinite(value) or not .25 <= value < 2**31 or value * 4 != int(value * 4):
        raise ValueError('invalid_vcpus')
    return int(value * 4)


def apply_quota(peer, request, expected_container):
    if set(request) != {'pid', 'vm_id', 'vcpus'}: raise ValueError('invalid_request')
    pid = request['pid']; vm = request['vm_id']; q = quarters(request['vcpus'])
    if type(pid) is not int or pid <= 1 or not isinstance(vm, str) or not re.fullmatch(r'vm_[0-9a-f]{32}', vm):
        raise ValueError('invalid_request')
    # Docker's cgroup is root-owned. Pin its full container ID using the
    # operator-supplied container name; an unrelated process with UID 10001 is not enough.
    container = expected_container
    if not re.fullmatch(r'[0-9a-f]{64}', container): raise ValueError('invalid_container_pin')
    group = Path(f'/proc/{peer}/cgroup').read_text().strip().split('0::')[-1]
    base = group.split('/controller')[0]
    if not re.fullmatch(r'/system.slice/docker-' + container + r'\.scope', base):
        raise ValueError('unauthorized_worker')
    # Translate only through the authenticated peer's children, not a global
    # host PID scan. Descendants must be QEMU in the same container cgroup.
    candidates = Path(f'/proc/{peer}/task')
    children = set()
    for task in candidates.iterdir():
        children.update(int(x) for x in (task/'children').read_text().split())
    target = None
    for child in children:
        try:
            status = Path(f'/proc/{child}/status').read_text()
            ns = next(line for line in status.splitlines() if line.startswith('NSpid:')).split()[1:]
            if int(ns[-1]) == pid: target = child; break
        except (FileNotFoundError, StopIteration): continue
    if target is None: raise ValueError('not_worker_child')
    exe = os.readlink(f'/proc/{target}/exe')
    args = Path(f'/proc/{target}/cmdline').read_bytes().split(b'\0')
    if not exe.endswith('/qemu-system-x86_64') or b'-name' not in args or args[args.index(b'-name')+1] != vm.encode():
        raise ValueError('not_owned_qemu')
    target_group = Path(f'/proc/{target}/cgroup').read_text().strip().split('0::')[-1]
    if target_group not in (base, base+'/controller', base+'/'+vm): raise ValueError('wrong_cgroup')
    root = Path('/sys/fs/cgroup'+base)
    controller = root/'controller'; controller.mkdir(exist_ok=True)
    # cgroup v2 requires no processes in a parent distributing CPU bandwidth.
    for process in (root/'cgroup.procs').read_text().split():
        try: (controller/'cgroup.procs').write_text(process)
        except ProcessLookupError: pass
    (root/'cgroup.subtree_control').write_text('+cpu')
    leaf = root/vm; leaf.mkdir(exist_ok=True)
    (leaf/'cpu.max').write_text(f'{q*25000} 100000')
    # The runner holds this child unreaped for the RPC lifetime, preventing PID
    # reuse. QEMU starts with -S and cannot run guest code before this succeeds.
    (leaf/'cgroup.procs').write_text(str(target))
    if (leaf/'cpu.max').read_text().strip() != f'{q*25000} 100000': raise ValueError('quota_not_applied')
    for old in root.glob('vm_*'):
        if old != leaf:
            try: old.rmdir()
            except OSError: pass
    return {'ok': True, 'quota_us': q*25000, 'period_us': 100000}


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        self.connection.settimeout(5)
        try:
            peer, uid, _ = struct.unpack('3i', self.connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            if uid != self.server.allowed_uid: raise ValueError('unauthorized_peer')
            line = self.rfile.readline(2049)
            if len(line) > 2048: raise ValueError('request_too_large')
            container = subprocess.check_output(['docker','inspect','--format','{{.Id}}',self.server.container_name],text=True,timeout=3).strip()
            result = apply_quota(peer, json.loads(line), container)
        except Exception:
            result = {'ok': False, 'error': 'cpu_quota_unavailable'}
        self.wfile.write(json.dumps(result).encode()+b'\n')


def main():
    p = argparse.ArgumentParser(); p.add_argument('--socket', required=True)
    p.add_argument('--container-name', required=True); p.add_argument('--uid', type=int, default=10001)
    args = p.parse_args(); path = Path(args.socket)
    path.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
    path.unlink(missing_ok=True)
    with socketserver.UnixStreamServer(str(path), Handler) as server:
        server.allowed_uid = args.uid; server.container_name = args.container_name
        os.chown(path, 0, args.uid); os.chmod(path, 0o660)
        server.serve_forever()


if __name__ == '__main__': main()
