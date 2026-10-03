#!/usr/bin/env python3
"""Start, resume or gracefully stop one VM via the authenticated worker RPC."""
import argparse
import json
from pathlib import Path
import re
import secrets
import time
import urllib.request


def main(root,project,vm,action):
    if not re.fullmatch(r'prj_[0-9a-f]{24}',project) or not re.fullmatch(r'vm_[0-9a-f]{32}',vm):
        raise ValueError('invalid_vm_identity')
    root=Path(root).resolve()
    meta=json.loads((root/'data/gap-compose/worker/vm/catalog'/f'{project}.json').read_text())
    expected={'start':'stopped','resume':'hibernated','stop':'running','hibernate':'running'}[action]
    if meta.get('project_id')!=project or meta.get('vm_id')!=vm or meta.get('state')!=expected:
        raise ValueError('vm_not_'+expected)
    token=(root/'data/gap-compose/config/service.token').read_text().strip()
    owner=meta['owner_did']

    def rpc(action,method,body):
        request=urllib.request.Request('http://172.17.0.1:8092/rpc',
            data=json.dumps({'project_id':project,'owner_did':owner,'action':action,
                             'method':method,'body':body}).encode(),
            headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
        with urllib.request.urlopen(request,timeout=15) as response:
            return json.loads(response.read(65536))

    job=rpc('vm/'+action,'POST',{'request_id':secrets.token_hex(16),'vm_id':vm})
    if job.get('status') not in ('queued','running','succeeded'):
        raise RuntimeError('operation_not_accepted')
    deadline=time.monotonic()+180
    while time.monotonic()<deadline:
        status=rpc('jobs/'+job['job_id'],'GET',{})
        if status.get('status')=='succeeded':
            verb={'start':'started','resume':'resumed','stop':'gracefully stopped','hibernate':'hibernated'}[action]
            print('VM '+verb+' through normal worker policy')
            return
        if status.get('status') in ('failed','interrupted'):
            raise RuntimeError('vm_'+action+'_failed:'+str(status.get('result',{}).get('error','unknown')))
        time.sleep(.5)
    raise TimeoutError('vm_'+action+'_timeout')


if __name__=='__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('--root',required=True)
    parser.add_argument('--project',required=True)
    parser.add_argument('--vm',required=True)
    parser.add_argument('--action',choices=('start','resume','stop','hibernate'),required=True)
    args=parser.parse_args()
    main(args.root,args.project,args.vm,args.action)
