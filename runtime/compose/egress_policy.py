"""Host-only, fail-closed egress policy for anonymous QEMU processes.

The CPU broker calls this while QEMU is paused with ``-S`` and before moving
it into its per-VM cgroup.  All rules are installed inside the compose edge's
network namespace, where QEMU's slirp sockets originate.  The worker has no
NET_ADMIN capability and cannot change these rules.
"""
import os
from pathlib import Path
import re
import shlex
import subprocess


POLICY = 'free_web_v1'
BYTE_BUDGET = 10 * 1024 ** 3
BLOCKED_IPV4 = (
    '0.0.0.0/8', '10.0.0.0/8', '100.64.0.0/10', '127.0.0.0/8',
    '169.254.0.0/16', '172.16.0.0/12', '192.0.0.0/24',
    '192.0.2.0/24', '192.168.0.0/16', '198.18.0.0/15',
    '198.51.100.0/24', '203.0.113.0/24', '224.0.0.0/4',
    '240.0.0.0/4',
)
DNS_IPV4 = ('1.1.1.1', '1.0.0.1', '8.8.8.8', '8.8.4.4')


def run(edge_pid, family, *arguments):
    binary = 'iptables' if family == 4 else 'ip6tables'
    return subprocess.run(
        ['nsenter', '-t', str(edge_pid), '-n', binary, '-w', '3', *arguments],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=8,
    )


def required(edge_pid, family, *arguments):
    result = run(edge_pid, family, *arguments)
    if result.returncode:
        raise RuntimeError('egress_firewall_unavailable')


def chains(vm_id):
    if not re.fullmatch(r'vm_[0-9a-f]{32}', vm_id):
        raise ValueError('invalid_vm_id')
    tag = vm_id[3:19]
    return 'GAPF1' + tag, 'GAPA1' + tag, 'GAP6F1' + tag


def ensure_chain(edge_pid, family, name):
    if run(edge_pid, family, '-S', name).returncode:
        required(edge_pid, family, '-N', name)
    # This chain belongs to one VM and must have no live process during
    # installation.  Clearing a stale policy also resets its one-hour quota.
    required(edge_pid, family, '-F', name)


def remove_output_references(edge_pid, family, name, vm_id):
    """Remove this VM's stale jumps, including those from old worker cgroups."""
    listing = run(edge_pid, family, '-S', 'OUTPUT')
    if listing.returncode:
        raise RuntimeError('egress_firewall_unavailable')
    for line in listing.stdout.splitlines():
        try:
            rule = shlex.split(line)
        except ValueError:
            raise RuntimeError('egress_firewall_unavailable') from None
        if len(rule) < 4 or rule[:2] != ['-A', 'OUTPUT'] or rule[-2:] != ['-j', name]:
            continue
        if '--path' not in rule or '-m' not in rule or rule[rule.index('-m')+1] != 'cgroup':
            raise RuntimeError('unexpected_egress_firewall_rule')
        group = rule[rule.index('--path')+1]
        if not re.fullmatch(r'/system\.slice/docker-[0-9a-f]{64}\.scope/' + re.escape(vm_id), group):
            raise RuntimeError('unexpected_egress_firewall_rule')
        members = Path('/sys/fs/cgroup' + group) / 'cgroup.procs'
        if members.exists() and members.read_text().strip():
            raise RuntimeError('vm_still_running')
        required(edge_pid, family, '-D', 'OUTPUT', *rule[2:])


def install(vm_id, group, target_pid, edge_container='gap-compose-compose-edge-1'):
    """Install policy before a QEMU child can send any guest packet."""
    main, public, ipv6 = chains(vm_id)
    if not re.fullmatch(r'/system\.slice/docker-[0-9a-f]{64}\.scope/vm_[0-9a-f]{32}', group):
        raise ValueError('invalid_vm_cgroup')
    if not group.endswith('/' + vm_id):
        raise ValueError('wrong_vm_cgroup')
    members = (Path('/sys/fs/cgroup' + group) / 'cgroup.procs').read_text().split()
    if members:
        raise ValueError('vm_cgroup_in_use')
    result = subprocess.run(['docker', 'inspect', '--format', '{{.State.Pid}}', edge_container],
                            capture_output=True, text=True, timeout=3)
    if result.returncode or not result.stdout.strip().isdigit():
        raise RuntimeError('egress_edge_unavailable')
    edge_pid = int(result.stdout.strip())
    if edge_pid <= 1 or os.stat(f'/proc/{edge_pid}/ns/net').st_ino != os.stat(f'/proc/{target_pid}/ns/net').st_ino:
        raise RuntimeError('egress_namespace_mismatch')

    for family, name in ((4, main), (6, ipv6)):
        remove_output_references(edge_pid, family, name, vm_id)

    ensure_chain(edge_pid, 4, main)
    ensure_chain(edge_pid, 4, public)
    ensure_chain(edge_pid, 6, ipv6)

    # Existing loopback connections are the SSH/preview host-forwards, not
    # guest-initiated connections to the host. Docker's embedded resolver
    # DNATs 127.0.0.11:53 to a container-specific ephemeral port before the
    # filter OUTPUT hook; permit only that dedicated loopback destination.
    required(edge_pid, 4, '-A', main, '-d', '127.0.0.0/8', '-m', 'conntrack',
             '--ctstate', 'ESTABLISHED,RELATED', '-j', 'ACCEPT')
    required(edge_pid, 4, '-A', main, '-d', '127.0.0.11',
             '-m', 'limit', '--limit', '60/second', '--limit-burst', '120', '-j', 'ACCEPT')
    required(edge_pid, 4, '-A', main, '-m', 'quota', '--quota', str(BYTE_BUDGET), '-j', public)
    required(edge_pid, 4, '-A', main, '-j', 'REJECT')

    for cidr in BLOCKED_IPV4:
        required(edge_pid, 4, '-A', public, '-d', cidr, '-j', 'REJECT')
    for address in DNS_IPV4:
        required(edge_pid, 4, '-A', public, '-d', address, '-p', 'udp', '--dport', '53',
                 '-m', 'limit', '--limit', '60/second', '--limit-burst', '120', '-j', 'ACCEPT')
        required(edge_pid, 4, '-A', public, '-d', address, '-p', 'tcp', '--dport', '53', '-j', 'ACCEPT')
    for port in ('80', '443'):
        required(edge_pid, 4, '-A', public, '-p', 'tcp', '--dport', port,
                 '-m', 'conntrack', '--ctstate', 'ESTABLISHED', '-j', 'ACCEPT')
        required(edge_pid, 4, '-A', public, '-p', 'tcp', '--dport', port, '--syn',
                 '-m', 'limit', '--limit', '12/second', '--limit-burst', '40', '-j', 'ACCEPT')
    required(edge_pid, 4, '-A', public, '-j', 'REJECT')
    required(edge_pid, 6, '-A', ipv6, '-j', 'REJECT')

    if run(edge_pid, 4, '-C', 'OUTPUT', '-m', 'cgroup', '--path', group, '-j', main).returncode:
        required(edge_pid, 4, '-I', 'OUTPUT', '1', '-m', 'cgroup', '--path', group, '-j', main)
    if run(edge_pid, 6, '-C', 'OUTPUT', '-m', 'cgroup', '--path', group, '-j', ipv6).returncode:
        required(edge_pid, 6, '-I', 'OUTPUT', '1', '-m', 'cgroup', '--path', group, '-j', ipv6)
    for family, name in ((4, main), (6, ipv6)):
        if run(edge_pid, family, '-C', 'OUTPUT', '-m', 'cgroup', '--path', group, '-j', name).returncode:
            raise RuntimeError('egress_firewall_not_active')


def release(vm_id, group, edge_container='gap-compose-compose-edge-1'):
    """Remove only this VM's firewall rules after its process has exited."""
    main, public, ipv6 = chains(vm_id)
    if not group.endswith('/' + vm_id):
        raise ValueError('wrong_vm_cgroup')
    members = Path('/sys/fs/cgroup' + group) / 'cgroup.procs'
    if members.exists() and members.read_text().strip():
        raise RuntimeError('vm_still_running')
    result = subprocess.run(['docker', 'inspect', '--format', '{{.State.Pid}}', edge_container],
                            capture_output=True, text=True, timeout=3)
    if result.returncode or not result.stdout.strip().isdigit():
        return
    edge_pid = int(result.stdout.strip())
    for family, name in ((4, main), (6, ipv6)):
        remove_output_references(edge_pid, family, name, vm_id)
    for family, name in ((4, main), (4, public), (6, ipv6)):
        if not run(edge_pid, family, '-S', name).returncode:
            required(edge_pid, family, '-F', name)
            required(edge_pid, family, '-X', name)
