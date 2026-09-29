#!/usr/bin/env python3
"""Operator-only live check of the anonymous VM host egress firewall.

This creates a temporary cgroup and a network-namespace-scoped probe. It does
not reserve a free VM or touch a customer's disk or admission record.
"""
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'runtime/compose'))
from egress_policy import install, release


PROBE = r'''
import socket
import ssl
import sys

print('READY', flush=True)
sys.stdin.readline()
results = {}
query = bytes.fromhex('123401000001000000000000076578616d706c6503636f6d0000010001')
try:
    dns = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    dns.settimeout(4)
    dns.sendto(query, ('127.0.0.11', 53))
    reply, _ = dns.recvfrom(512)
    results['docker_dns'] = len(reply) > 12 and reply[:2] == query[:2]
except OSError:
    results['docker_dns'] = False
try:
    remote = socket.create_connection(('1.1.1.1', 443), 5)
    tls = ssl.create_default_context().wrap_socket(remote, server_hostname='cloudflare.com')
    tls.sendall(b'HEAD / HTTP/1.1\r\nHost: cloudflare.com\r\nConnection: close\r\n\r\n')
    results['https'] = tls.recv(128).startswith(b'HTTP/')
    tls.close()
except OSError:
    results['https'] = False
for label, address, port in (('metadata_blocked', '169.254.169.254', 80),
                             ('smtp_blocked', '1.1.1.1', 25),
                             ('private_blocked', '10.0.0.1', 443)):
    try:
        sock = socket.create_connection((address, port), 2)
        sock.close()
        results[label] = False
    except OSError:
        results[label] = True
print('RESULT ' + __import__('json').dumps(results), flush=True)
'''


def inspect(container, template):
    return subprocess.check_output(['docker', 'inspect', '-f', template, container], text=True).strip()


def main():
    runner_id = inspect('gap-compose-compose-runner-1', '{{.Id}}')
    runner_pid = int(inspect('gap-compose-compose-runner-1', '{{.State.Pid}}'))
    edge_pid = int(inspect('gap-compose-compose-edge-1', '{{.State.Pid}}'))
    base = Path(f'/proc/{runner_pid}/cgroup').read_text().strip().split('0::')[-1].split('/controller')[0]
    if base != f'/system.slice/docker-{runner_id}.scope':
        raise RuntimeError('unexpected_runner_cgroup')
    vm_id = 'vm_' + secrets.token_hex(16)
    group = base + '/' + vm_id
    leaf = Path('/sys/fs/cgroup' + group)
    leaf.mkdir()
    child = None
    try:
        child = subprocess.Popen(['nsenter', '-t', str(edge_pid), '-n', sys.executable, '-u', '-c', PROBE],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True)
        if child.stdout.readline().strip() != 'READY':
            raise RuntimeError('probe_not_ready')
        install(vm_id, group, child.pid)
        (leaf / 'cgroup.procs').write_text(str(child.pid))
        child.stdin.write('\n')
        child.stdin.flush()
        result = child.stdout.readline().strip()
        if not result.startswith('RESULT '):
            raise RuntimeError('probe_failed')
        values = json.loads(result[7:])
        print(json.dumps(values, sort_keys=True))
        if not all(values.values()):
            raise RuntimeError('egress_policy_check_failed')
    finally:
        if child is not None:
            if child.poll() is None:
                child.terminate()
            child.wait(timeout=10)
        release(vm_id, group)
        leaf.rmdir()


if __name__ == '__main__':
    main()
