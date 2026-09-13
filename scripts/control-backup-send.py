#!/usr/bin/env python3
"""Installed on the primary control host; only encrypted archives leave it."""
import hashlib
import json
from pathlib import Path
import subprocess
import time
import os
os.umask(0o077)
root=Path('/var/lib/gap-control-backup');archive=root/(str(time.time_ns())+'.gapenc')
result=subprocess.run(['/usr/bin/python3','/opt/gap-backup/control-backup.py','backup',
    '--database','/opt/app/gap-node-01/data/gap-control/state/authority.sqlite',
    '--config','/opt/app/gap-node-01/data/gap-control/config','--public-key','/opt/gap-backup/recovery.pub.pem',
    '--output',str(archive)],capture_output=True,text=True,check=True)
created=json.loads(result.stdout)
with archive.open('rb') as f:
    transfer=subprocess.run(['ssh','-i','/opt/gap-backup/transfer-key','-o','BatchMode=yes',
        '-o','StrictHostKeyChecking=yes','-o','UserKnownHostsFile=/opt/gap-backup/known_hosts',
        '-o','ConnectTimeout=10','-o','ServerAliveInterval=15','-o','ServerAliveCountMax=3',
        'gapbackup@159.195.123.24'],stdin=f,capture_output=True,timeout=180,check=True)
receipt=json.loads(transfer.stdout)
if receipt['sha256']!=created['sha256'] or receipt['bytes']!=created['bytes']:raise ValueError('transfer verification failed')
status=dict(completed_at=int(time.time()),local_file=archive.name,remote_file=receipt['file'],sha256=receipt['sha256'],bytes=receipt['bytes'])
tmp=root/'latest.json.tmp';tmp.write_text(json.dumps(status));tmp.replace(root/'latest.json')
print(json.dumps(status))

# Prune only this service's old encrypted spool after confirmed delivery.
for p in sorted(root.glob('*.gapenc'),key=lambda p:p.stat().st_mtime)[:-2]:
    if p.stat().st_mtime<time.time()-7*86400:p.unlink()
