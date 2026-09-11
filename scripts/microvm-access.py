#!/usr/bin/env python3
"""Manage live microVM access (native programs and optional Compose) on the operator host without restarting GAP."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import stat
import tempfile


def change(path, action, agent=None, vcpus=None, memory_mib=None, always_on=None, max_vms=None, disk_gib=None):
    if agent is not None and not re.fullmatch(r'did:gap:[0-9a-f]{64}', agent):
        raise ValueError('agent must be an exact did:gap:<64 lowercase hex> identity')
    path = Path(path).absolute()
    if not path.parent.is_dir():
        raise ValueError('approval directory must already exist')
    if path.is_symlink():
        raise ValueError('approval file must not be a symlink')
    lock_fd = os.open(str(path) + '.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(lock_fd, 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            raw = path.read_bytes()
            metadata = path.stat()
        except FileNotFoundError:
            if action != 'grant':
                raise ValueError('approval file does not exist')
            raw, metadata = b'{"agents":[]}', None
        if len(raw) > 65536:
            raise ValueError('approval file exceeds node limit')
        data = json.loads(raw)
        if (not isinstance(data, dict) or not {'agents'} <= set(data) <= {'agents', 'quotas', 'always_on_agents'} or not isinstance(data['agents'], list)
                or any(not isinstance(did, str) or not re.fullmatch(r'did:gap:[0-9a-f]{64}', did) for did in data['agents'])):
            raise ValueError('invalid approval file; refusing to overwrite it')
        always = data.get('always_on_agents', [])
        if not isinstance(always,list) or any(did not in data['agents'] for did in always):
            raise ValueError('invalid always-on approval')
        if always_on is not None and type(always_on) is not bool: raise ValueError('invalid always-on flag')
        if always_on is not None and action not in ('grant','set-always-on'): raise ValueError('always-on flag requires grant or set-always-on')
        if action=='set-always-on' and (agent not in data['agents'] or always_on is None): raise ValueError('set-always-on requires approved agent and --always-on yes|no')
        quotas = data.get('quotas', {})
        def valid_quota(q):
            return (isinstance(q, dict) and {'vcpus', 'memory_mib'} <= set(q) <= {'vcpus', 'memory_mib', 'max_vms', 'disk_gib'}
                    and all(type(v) is int and 0 < v < 2**31 for v in q.values()))
        if not isinstance(quotas, dict) or any(did not in data['agents'] or not valid_quota(q) for did, q in quotas.items()):
            raise ValueError('invalid quota store')
        if action not in ('grant', 'revoke', 'list', 'set-quota', 'set-always-on'):
            raise ValueError('invalid action')
        if any(v is not None and (type(v) is not int or not 0 < v < 2**31) for v in (vcpus, memory_mib, max_vms, disk_gib)):
            raise ValueError('quotas must be positive integers below 2**31')
        if action in ('list', 'revoke') and (vcpus is not None or memory_mib is not None or max_vms is not None or disk_gib is not None):
            raise ValueError('quota flags require grant or set-quota')
        if action == 'set-quota' and (agent not in data['agents'] or (vcpus is None and memory_mib is None and max_vms is None and disk_gib is None)):
            raise ValueError('set-quota requires an approved agent and at least one quota flag')
        agents = set(data['agents'])
        original = json.dumps(data, sort_keys=True)
        if action == 'list':
            return {'agents': sorted(agents), 'quotas': {did: dict({'vcpus': 2, 'memory_mib': 4096, 'max_vms': 1}, **quotas.get(did, {})) for did in sorted(agents)}, 'always_on_agents': sorted(always)}
        before = set(agents)
        if action == 'grant':
            agents.add(agent)
        elif action == 'revoke':
            agents.discard(agent)
            quotas.pop(agent, None)
        if vcpus is not None or memory_mib is not None or max_vms is not None or disk_gib is not None:
            q = dict(quotas.get(agent, {'vcpus': 2, 'memory_mib': 4096}))
            if vcpus is not None:
                q['vcpus'] = vcpus
            if memory_mib is not None:
                q['memory_mib'] = memory_mib
            if max_vms is not None:
                q['max_vms'] = max_vms
            if disk_gib is not None:
                q['disk_gib'] = disk_gib
            quotas[agent] = q
        if action=='revoke': always=[did for did in always if did!=agent]
        if always_on is True and agent not in always: always.append(agent)
        if always_on is False: always=[did for did in always if did!=agent]
        updated = {'agents': sorted(agents)}
        if always: updated['always_on_agents']=sorted(always)
        if quotas:
            updated['quotas'] = quotas
        changed = json.dumps(updated, sort_keys=True) != original
        encoded = (json.dumps(updated) + '\n').encode()
        if len(encoded) > 65536:
            raise ValueError('approval file would exceed node limit')
        fd, temporary = tempfile.mkstemp(prefix='.' + path.name + '.', dir=path.parent)
        try:
            with os.fdopen(fd, 'wb') as out:
                if metadata:
                    if os.geteuid() == 0:
                        os.fchown(out.fileno(), metadata.st_uid, metadata.st_gid)
                    os.fchmod(out.fileno(), stat.S_IMODE(metadata.st_mode))
                out.write(encoded)
                out.flush()
                os.fsync(out.fileno())
            os.replace(temporary, path)
            directory = os.open(path.parent, os.O_DIRECTORY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
        return {'agent': agent, 'approved': agent in agents, 'changed': changed, 'quota': dict({'vcpus': 2, 'memory_mib': 4096, 'max_vms': 1}, **quotas.get(agent, {})) if agent in agents else None, 'restart_required': False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--file', default='data/gap-node/compose-agents.json', help='shared microVM approval store (legacy GAP_COMPOSE_APPROVALS_FILE)')
    parser.add_argument('action', choices=('grant', 'revoke', 'list', 'set-quota', 'set-always-on'))
    parser.add_argument('agent', nargs='?')
    parser.add_argument('--vcpus', type=int, help='Total allocated vCPUs across this agent’s VMs')
    parser.add_argument('--memory-mib', type=int, help='Total allocated RAM in MiB across this agent’s VMs')
    parser.add_argument('--max-vms', type=int, help='Maximum number of non-destroyed VMs, default 1')
    parser.add_argument('--disk-gib', type=int, help='Optional total provisioned disk quota, including retained volumes')
    parser.add_argument('--always-on', choices=('yes','no'))
    args = parser.parse_args()
    if (args.action == 'list') != (args.agent is None):
        parser.error('grant/revoke/set-quota require one agent DID; list takes none')
    try:
        print(json.dumps(change(args.file, args.action, args.agent, args.vcpus, args.memory_mib, None if args.always_on is None else args.always_on=='yes', args.max_vms, args.disk_gib)))
    except (OSError, ValueError) as error:
        parser.exit(1, str(error) + '\n')


if __name__ == '__main__':
    main()
