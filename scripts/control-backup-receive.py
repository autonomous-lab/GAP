#!/usr/bin/env python3
"""Forced SSH command: append a bounded encrypted archive, no shell or deletion."""
import hashlib
import json
import os
from pathlib import Path
import secrets
import sys
import time
os.umask(0o077)
if os.environ.get('SSH_ORIGINAL_COMMAND'): raise SystemExit('commands disabled')
root=Path('/var/lib/gap-backup/archives');name=str(time.time_ns())+'-'+secrets.token_hex(8)+'.gapenc'
p=root/name;size=0;digest=hashlib.sha256()
try:
    with p.open('xb') as f:
        first=True
        while chunk:=sys.stdin.buffer.read(65536):
            if first and not chunk.startswith(b'GAPCONTROL1\0'):raise ValueError('invalid envelope')
            first=False;size+=len(chunk)
            if size>128*1024*1024:raise ValueError('size limit')
            digest.update(chunk);f.write(chunk)
        if size<64:raise ValueError('empty envelope')
        f.flush();os.fsync(f.fileno())
    fd=os.open(root,os.O_RDONLY|os.O_DIRECTORY);os.fsync(fd);os.close(fd)
    for old in sorted(root.glob('*.gapenc'),key=lambda p:p.stat().st_mtime)[:-2]:
        if old.stat().st_mtime<time.time()-30*86400:old.unlink()
    print(json.dumps(dict(file=name,sha256=digest.hexdigest(),bytes=size)))
except Exception:
    p.unlink(missing_ok=True)
    raise
