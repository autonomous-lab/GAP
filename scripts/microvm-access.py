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


def change(path, action, agent=None):
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
        if (not isinstance(data, dict) or set(data) != {'agents'} or not isinstance(data['agents'], list)
                or any(not isinstance(did, str) or not re.fullmatch(r'did:gap:[0-9a-f]{64}', did) for did in data['agents'])):
            raise ValueError('invalid approval file; refusing to overwrite it')
        agents = set(data['agents'])
        if action == 'list':
            return {'agents': sorted(agents)}
        before = set(agents)
        if action == 'grant':
            agents.add(agent)
        else:
            agents.discard(agent)
        encoded = (json.dumps({'agents': sorted(agents)}) + '\n').encode()
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
        return {'agent': agent, 'approved': agent in agents, 'changed': agents != before, 'restart_required': False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--file', default='data/gap-node/compose-agents.json', help='shared microVM approval store (legacy GAP_COMPOSE_APPROVALS_FILE)')
    parser.add_argument('action', choices=('grant', 'revoke', 'list'))
    parser.add_argument('agent', nargs='?')
    args = parser.parse_args()
    if (args.action == 'list') != (args.agent is None):
        parser.error('grant/revoke require one agent DID; list takes none')
    try:
        print(json.dumps(change(args.file, args.action, args.agent)))
    except (OSError, ValueError) as error:
        parser.exit(1, str(error) + '\n')


if __name__ == '__main__':
    main()
