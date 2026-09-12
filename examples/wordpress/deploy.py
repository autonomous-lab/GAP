#!/usr/bin/env python3
"""Create and publish a new WordPress MicroVM; rerun with the same private state."""
import argparse
import base64
import json
import os
from pathlib import Path
import re
import secrets
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from verify import verify


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs): return None


class Deployment:
    def __init__(self, node, project, token, path):
        self.node, self.project, self.token, self.path = node.rstrip('/'), project, token, path
        self.state = json.loads(path.read_text()) if path.exists() else {}
        if self.state and (self.state['node'] != self.node or self.state['project'] != project):
            raise RuntimeError('State belongs to a different node or project')
        self.opener = urllib.request.build_opener(NoRedirect)
        self.opener.addheaders = [("User-Agent", "GAP-WordPress/1.0")]

    def save(self):
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        temporary = self.path.with_suffix('.tmp')
        with temporary.open('w') as output:
            os.chmod(temporary, 0o600)
            json.dump(self.state, output)
            output.flush()
            os.fsync(output.fileno())
        temporary.replace(self.path)

    def api(self, method, suffix, body=None):
        req = urllib.request.Request(self.node+'/v1/cloud/projects/'+self.project+suffix,
            method=method, data=json.dumps(body).encode() if body is not None else None,
            headers={'Authorization':'Bearer '+self.token,'Content-Type':'application/json'})
        try:
            with self.opener.open(req, timeout=30) as response: return json.load(response)
        except urllib.error.HTTPError as error:
            # Do not echo server response bodies; guest logs can contain secrets.
            raise RuntimeError('API HTTP '+str(error.code)+' on '+suffix.split('?')[0]+'. Rerun with the same state after inspecting the operation.') from None

    def job(self, job_id):
        deadline = time.monotonic()+720
        while time.monotonic()<deadline:
            job = self.api('GET','/vm/jobs/'+job_id)
            if job['status'] not in ('queued','running'): return job
            time.sleep(2)
        raise RuntimeError('Job still pending: '+job_id+'. Rerun with the same state.')

    def operation(self, name, method, suffix, fields):
        operations = self.state.setdefault('operations',{})
        if name not in operations:
            operations[name] = {'method':method,'suffix':suffix,'body':dict(fields,request_id=uuid.uuid4().hex)}
            self.save()
        operation = operations[name]
        if 'result' in operation: return operation['result']
        if 'job_id' not in operation:
            submitted = self.api(operation['method'],operation['suffix'],operation['body'])
            operation['job_id'] = submitted['job_id'];self.save()
        result = self.job(operation['job_id'])
        if result['status'] != 'succeeded':
            code = result.get('result',{}).get('error','inspect_job')
            # Codes are allowlisted identifiers, never print arbitrary guest output.
            if not isinstance(code,str) or not re.fullmatch('[a-z_]+',code): code='inspect_job'
            raise RuntimeError(name+' failed: '+code+'; job '+operation['job_id']+'. No automatic replay or VM deletion.')
        operation['result'] = result['result'];self.save()
        return operation['result']

    def ready(self, vm):
        deadline = time.monotonic()+180
        reason = 'not_probed'
        previous_reason = None
        while time.monotonic()<deadline:
            submitted = self.api('POST','/vm/readiness',{'vm_id':vm,'request_id':uuid.uuid4().hex})
            job = self.job(submitted['job_id'])
            result = job.get('result') or {}
            if result.get('ready'): return
            reason = result.get('reason',result.get('error','probe_failed'))
            if reason != previous_reason and isinstance(reason,str) and re.fullmatch('[a-z_]+',reason):
                print('Waiting for guest: '+reason,flush=True)
                previous_reason = reason
            if reason in ('guest_ssh_host_key_mismatch','guest_ssh_authentication_failed','vm_not_running'):
                break
            time.sleep(2)
        raise RuntimeError('Guest not ready: '+str(reason)+'. VM preserved; inspect readiness before retrying.')

    def run(self, email, title):
        if not self.state:
            self.state = {'node':self.node,'project':self.project,'admin_email':email,'title':title,
                'visitor_user':'visitor','visitor_password':secrets.token_urlsafe(24),
                'admin_user':'wpadmin','admin_password':secrets.token_urlsafe(24),
                'db_password':secrets.token_urlsafe(24),'db_root_password':secrets.token_urlsafe(24)}
            self.save()
        if 'vm_id' not in self.state:
            print('Creating a separate, stopped VM (1 vCPU, 1 GiB RAM, 8 GiB disk).',flush=True)
            result = self.operation('create','POST','/vms',{'vcpus':1,'memory_mib':1024,'disk_gib':8,'start':False})
            self.state['vm_id'] = result['vm']['vm_id'];self.save()
        vm = self.state['vm_id']
        ingress = self.api('GET','/vm/ingress?vm_id='+vm)
        self.state['url'] = ingress['url'];self.save()
        # Keep ingress disabled until installation is complete. Explicit WP_PUBLIC_URL
        # avoids depending on GAP_PUBLIC_URL, which is empty while unpublished.
        print('Starting VM and waiting for guest control and Docker.',flush=True)
        self.operation('start','POST','/vm/start',{'vm_id':vm})
        self.ready(vm)
        env = {'MARIADB_DATABASE':'wordpress','MARIADB_USER':'wordpress',
            'MARIADB_PASSWORD':self.state['db_password'],'MARIADB_ROOT_PASSWORD':self.state['db_root_password'],
            'WORDPRESS_DB_HOST':'database:3306','WORDPRESS_DB_USER':'wordpress',
            'WORDPRESS_DB_PASSWORD':self.state['db_password'],'WORDPRESS_DB_NAME':'wordpress',
            'WP_PUBLIC_URL':self.state['url'],'WP_ADMIN_USER':self.state['admin_user'],
            'WP_ADMIN_PASSWORD':self.state['admin_password'],'WP_ADMIN_EMAIL':self.state['admin_email'],
            'WP_TITLE':self.state['title']}
        if any('\n' in v or '\r' in v for v in env.values()): raise RuntimeError('Environment values must be single-line')
        root = Path(__file__).resolve().parent
        sources = {name:(root/name).read_bytes() for name in ('compose.yaml','config.php','install.sh','Dockerfile.wordpress','Dockerfile.install')}
        sources['runtime.env'] = ''.join(k+'='+v+'\n' for k,v in env.items()).encode()
        print('Installing WordPress with unpublished ingress.',flush=True)
        self.operation('install','POST','/stack/releases',{'vm_id':vm,'compose_file':'compose.yaml',
            'files':{k:base64.b64encode(v).decode() for k,v in sources.items()}})
        self.api('PUT','/vm/http-access',{'vm_id':vm,'username':self.state['visitor_user'],'password':self.state['visitor_password']})
        print('Publishing port 8001 behind GAP visitor authentication.',flush=True)
        self.operation('publish','PUT','/vm/ingress',{'vm_id':vm,'enabled':True,'guest_port':8001})
        ingress = self.api('GET','/vm/ingress?vm_id='+vm)
        if not ingress.get('access_ready'): raise RuntimeError('Ingress or visitor access is not configured')
        verify(self.state)
        print('WordPress URL: '+self.state['url'])
        print('Private state and separate visitor/WordPress credentials: '+str(self.path.resolve()))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--node',default=os.environ.get('GAP_NODE','https://gap.geta.team'))
    parser.add_argument('--project',required=True)
    parser.add_argument('--email',required=True,help='WordPress administrator email; no installation email is sent')
    parser.add_argument('--title',default='WordPress on GAP')
    parser.add_argument('--state',type=Path,required=True,help='Private JSON checkpoint outside source control; reuse it for retries')
    args = parser.parse_args()
    if not os.environ.get('GAP_TOKEN'): parser.error('Set GAP_TOKEN to your approved owner bearer')
    if not re.fullmatch(r'prj_[0-9a-f]{24}',args.project): parser.error('Invalid project ID')
    parsed = urllib.parse.urlsplit(args.node)
    if parsed.scheme != 'https' or not parsed.netloc or parsed.username or parsed.query or parsed.fragment or parsed.path not in ('','/'):
        parser.error('--node must be an HTTPS origin')
    os.umask(0o077)
    try: Deployment(args.node,args.project,os.environ['GAP_TOKEN'],args.state).run(args.email,args.title)
    except RuntimeError as error: raise SystemExit(str(error)) from None
    except Exception as error: raise SystemExit('Deployment interrupted ('+type(error).__name__+'); keep the private state and rerun.') from None


if __name__ == '__main__': main()
