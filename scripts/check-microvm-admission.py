#!/usr/bin/env python3
"""Deployment gate; read-only, private operator credential, no VM creation."""
import argparse
import json
from pathlib import Path
import urllib.request
import urllib.error

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--token-file',required=True)
a=p.parse_args()
request=urllib.request.Request('http://172.17.0.1:8092/operator',
    data=b'{"action":"admission-readiness"}',headers={
        'Authorization':'Bearer '+Path(a.token_file).read_text().strip(),
        'Content-Type':'application/json'})
try:
    with urllib.request.urlopen(request,timeout=5) as response:result=json.load(response)
except (OSError,ValueError):
    result={'ready':False,'errors':['worker_readiness_unavailable']}
print(json.dumps(result,indent=2))
raise SystemExit(0 if result.get('ready') is True else 1)
