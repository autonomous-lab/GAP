#!/usr/bin/env python3
"""Fill missing local runtime secrets in .env without rotating existing values."""
import argparse
import os
from pathlib import Path
import re
import secrets
import tempfile

KEYS = ('GAP_FUNCTION_SANDBOX_TOKEN', 'GAP_REALTIME_SECRET')


def initialize(path):
    path = Path(path)
    if path.is_symlink():
        raise ValueError('Refusing a symlink environment file')
    source = path.read_text() if path.exists() else ''
    lines = source.splitlines(keepends=True)
    added = []
    for key in KEYS:
        pattern = re.compile(r'^\s*(?:export\s+)?' + key + r'\s*=\s*(.*)$')
        matches = [(i, pattern.match(line)) for i, line in enumerate(lines) if pattern.match(line)]
        if len(matches) > 1:
            raise ValueError('Duplicate environment key: ' + key)
        if matches and matches[0][1].group(1).strip().strip(chr(34)).strip(chr(39)):
            continue
        value = key + '=' + secrets.token_hex(32) + '\n'
        if matches:
            lines[matches[0][0]] = value
        else:
            if lines and not lines[-1].endswith('\n'):
                lines[-1] += '\n'
            lines.append(value)
        added.append(key)
    if added:
        fd, temporary = tempfile.mkstemp(prefix='.env-init-', dir=path.parent)
        try:
            with os.fdopen(fd, 'w') as stream:
                stream.write(''.join(lines)); stream.flush(); os.fsync(stream.fileno())
            if path.exists():
                stat = path.stat(); os.chown(temporary, stat.st_uid, stat.st_gid)
            os.replace(temporary, path)
        finally:
            if os.path.exists(temporary): os.unlink(temporary)
    print('Generated: ' + ', '.join(added) if added else 'Runtime secrets already configured; unchanged')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--env-file', default='.env')
    initialize(parser.parse_args().env_file)
