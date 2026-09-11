#!/usr/bin/env python3
"""Configure GAP's local Postfix relay without copying upstream credentials."""
import argparse
import fcntl
import os
from pathlib import Path
import re
import shlex
import stat
import tempfile


def configure(script, env_file):
    # Parse, never execute, the operator-owned provisioning script.
    values = [part.partition('=')[2] for part in shlex.split(Path(script).read_text())
              if part.partition('=')[0] == 'RELAYHOST_USERNAME']
    if len(values) != 1 or not re.fullmatch(r'[A-Za-z0-9._+%-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}', values[0]):
        raise ValueError('expected one bare RELAYHOST_USERNAME email in Postfix script')
    path = Path(env_file).absolute()
    if path.is_symlink():
        raise ValueError('refusing symlink environment file')
    with os.fdopen(os.open(str(path)+'.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600), 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        metadata = path.stat()
        if not stat.S_ISREG(metadata.st_mode):
            raise ValueError('environment must be a regular file')
        original = path.read_text()
        fields = {'GAP_SMTP_HOST': '172.17.0.1', 'GAP_SMTP_PORT': '25', 'GAP_SMTP_FROM': values[0]}
        lines = original.splitlines()
        seen = set()
        for i, line in enumerate(lines):
            key = line.partition('=')[0].strip().removeprefix('export ')
            if key in fields:
                if key in seen:
                    raise ValueError('duplicate SMTP setting')
                seen.add(key)
                lines[i] = key+'='+fields[key]
        lines.extend(key+'='+value for key, value in fields.items() if key not in seen)
        content = '\n'.join(lines)+'\n'
        if content != original:
            fd, temporary = tempfile.mkstemp(prefix='.gap-smtp-', dir=path.parent)
            try:
                with os.fdopen(fd, 'w') as out:
                    if os.geteuid() == 0:
                        os.fchown(out.fileno(), metadata.st_uid, metadata.st_gid)
                    os.fchmod(out.fileno(), 0o600)
                    out.write(content)
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
    return values[0]


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--script', default='/opt/elestio/startPostfix.sh')
    parser.add_argument('--env-file', default='.env')
    args = parser.parse_args()
    try:
        sender = configure(args.script, args.env_file)
        print('Configured local Postfix sender: '+sender)
        print('Email verification activation is unchanged; rebuild/recreate GAP to load configuration.')
    except (ValueError, OSError):
        parser.exit(1, 'Cannot configure SMTP; check script and environment file. No credentials logged.\n')
