"""Opt-in disposable KVM + real node/runner/Caddy WordPress acceptance test.
No production catalog, tokens, ports or volumes are mounted into this test.
"""
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from runner import Runner, handler_for


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); return sock.getsockname()[1]


def main():
    if os.environ.get('GAP_TEST_WORDPRESS') != '1': raise SystemExit('explicit KVM opt-in required')
    repo = Path(os.environ.get('GAP_TEST_REPO','/repo'))
    sys.path.insert(0,str(repo/'examples/wordpress'))
    from deploy import Deployment
    with tempfile.TemporaryDirectory(prefix='gap-wordpress-test-') as temporary:
        root=Path(temporary); runner=None; servers=[]; processes=[]
        node_port,runner_port,edge_port,tls_port=[port() for _ in range(4)]
        node='http://127.0.0.1:'+str(node_port); origin='https://localhost:'+str(tls_port)
        token='b'*64; edge_token='ab'*32
        (root/'token').write_text(token);(root/'edge-token').write_text(edge_token)
        approvals=root/'approvals.json';approvals.write_text('{"agents":[]}')
        env=dict(os.environ,GAP_ADDR='127.0.0.1:'+str(node_port),GAP_STORAGE='sqlite',
            GAP_SQLITE_PATH=str(root/'node.sqlite'),GAP_CLOUD_ROOT=str(root/'cloud'),
            GAP_COMPOSE_ENABLED='1',GAP_COMPOSE_APPROVALS_FILE=str(approvals),
            GAP_COMPOSE_RUNNER_URL='http://127.0.0.1:'+str(runner_port),GAP_COMPOSE_RUNNER_TOKEN=token,
            GAP_VM_EDGE_TOKEN=edge_token,GAP_MASTER_KEY='ac'*32,GAP_WORKERS='8')
        def api(method,path,bearer=None,body=None):
            req=urllib.request.Request(node+path,method=method,headers={'Content-Type':'application/json',
                **({'Authorization':'Bearer '+bearer} if bearer else {})},data=json.dumps(body).encode() if body is not None else None)
            try:response=urllib.request.urlopen(req,timeout=20)
            except urllib.error.HTTPError as error:response=error
            with response:return response.status,json.load(response)
        try:
            node_process=subprocess.Popen([os.environ['GAP_TEST_BINARY']],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            processes.append(node_process)
            for _ in range(100):
                try:
                    if api('GET','/health')[0]==200:break
                except OSError:pass
                time.sleep(.1)
            _,identity=api('POST','/v1/identity');bearer=identity['token'];owner=identity['did']
            _,project=api('POST','/v1/cloud/projects',bearer);project=project['project_id']
            approvals.write_text(json.dumps({'agents':[owner],'quotas':{owner:{'vcpus':3,'memory_mib':3072,'max_vms':2}}}))
            caddy_config=root/'caddy.json';admin_socket=str(root/'caddy.sock')
            caddy_config.write_text(json.dumps({'admin':{'listen':'unix/'+admin_socket}}))
            processes.append(subprocess.Popen(['caddy','run','--config',str(caddy_config)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL))
            for _ in range(100):
                if Path(admin_socket).exists():break
                time.sleep(.1)
            config={'approved_only':True,'token_file':str(root/'token'),'state_dir':str(root/'jobs'),'node_url':node,
                'hypervisor':{'state_dir':str(root/'vms'),'image_dir':'/images','diagnostic_serial':True},
                'ingress':{'dedicated_caddy':True,'public_url':origin,'admin_socket':admin_socket,
                    'http_port':edge_port,'admission_token_file':str(root/'edge-token')}}
            (root/'runner.json').write_text(json.dumps(config));runner=Runner(root/'runner.json')
            rpc_server=ThreadingHTTPServer(('127.0.0.1',runner_port),handler_for(runner));servers.append(rpc_server)
            threading.Thread(target=rpc_server.serve_forever,daemon=True).start()
            # TLS frontend mirrors nginx's auth_request contract, using the real
            # GAP admission endpoint and real private Caddy forwarding.
            class Front(BaseHTTPRequestHandler):
                def log_message(self,*args):pass
                def do_GET(self):self.proxy()
                def do_POST(self):self.proxy()
                def proxy(self):
                    admission=http.client.HTTPConnection('127.0.0.1',node_port,timeout=20)
                    admission.request('GET','/internal/vm-http-admission',headers={
                        'X-GAP-Edge-Token':edge_token,'X-GAP-Original-Host':'localhost',
                        'X-GAP-Original-Path':self.path.split('?')[0],'X-GAP-Original-URI':self.path,
                        'X-GAP-Original-Authorization':self.headers.get('Authorization','')})
                    response=admission.getresponse();headers=dict(response.getheaders());response.read();admission.close()
                    if response.status!=200:
                        self.send_response(response.status)
                        if response.status==401:self.send_header('WWW-Authenticate','Basic realm="GAP"')
                        self.end_headers();return
                    upstream=http.client.HTTPConnection('127.0.0.1',edge_port,timeout=120)
                    forwarded={k:v for k,v in self.headers.items() if k.lower() not in ('authorization','host','connection')}
                    forwarded.update({'Host':'localhost:'+str(tls_port),'X-GAP-VM-Admission':edge_token,
                        'X-GAP-VM-Identity':headers.get('X-GAP-VM-Identity','')})
                    body=self.rfile.read(int(self.headers.get('Content-Length','0')))
                    upstream.request(self.command,headers.get('X-GAP-VM-URI',self.path),body=body,headers=forwarded)
                    response=upstream.getresponse();data=response.read()
                    self.send_response(response.status)
                    for k,v in response.getheaders():
                        if k.lower() not in ('connection','transfer-encoding','content-length'):self.send_header(k,v)
                    self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data);upstream.close()
            subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1','-subj','/CN=localhost',
                '-addext','subjectAltName=DNS:localhost','-keyout',str(root/'tls.key'),'-out',str(root/'tls.crt')],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            os.environ['SSL_CERT_FILE']=str(root/'tls.crt')
            tls_server=ThreadingHTTPServer(('127.0.0.1',tls_port),Front)
            context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain(root/'tls.crt',root/'tls.key')
            tls_server.socket=context.wrap_socket(tls_server.socket,server_side=True);servers.append(tls_server)
            threading.Thread(target=tls_server.serve_forever,daemon=True).start()
            deployment=Deployment(node,project,bearer,root/'private-state.json')
            # Occupy then destroy the default slot. The example must use the
            # additional VM selector, not silently target a default replacement.
            default=runner.hypervisor.perform(project,owner,'vm/create',{'start':False})['vm']
            runner.hypervisor.perform(project,owner,'vm/destroy',{'vm_id':default['vm_id'],'delete_data':True,'confirm_data_loss':True})
            # Keep a stopped default and create an additional VM through the API.
            default=runner.hypervisor.perform(project,owner,'vm/create',{'start':False})['vm']
            deployment.run('test@example.invalid','Isolated GAP WordPress')
            assert runner.hypervisor.read(project,owner)['vm_id']==default['vm_id']
            assert runner.hypervisor.public(runner.hypervisor.read(project,owner))['state']=='stopped'
            selected=deployment.state['vm_id'];assert selected!=default['vm_id']
            suffix='/v1/cloud/projects/'+project+'/stack/status?vm_id='+selected
            status,job=api('POST',suffix,bearer,{'request_id':uuid.uuid4().hex})
            assert status==202,(status,job)
            result=deployment.job(job['job_id']);assert result['status']=='succeeded',result['status']
            assert result['result']['vm_id']==selected
            status,_=api('POST',suffix,bearer,{'request_id':uuid.uuid4().hex,'vm_id':default['vm_id']})
            assert status==400
            # Replay the complete bootstrap checkpoint: no new VM or release.
            deployment.run('test@example.invalid','Isolated GAP WordPress')
            assert len(runner.hypervisor.list(project,owner))==3
            print('PASS: cold boot without ingress, selected Compose, query selector/conflict, complete WordPress bootstrap and resumable replay',flush=True)
        except Exception:
            if runner and 'deployment' in locals():
                with runner.db() as db:
                    row=db.execute("SELECT result FROM jobs WHERE action='releases' ORDER BY created DESC LIMIT 1").fetchone()
                if row and row[0]:
                    result=json.loads(row[0]); diagnostic=result.get('output','')[-1000:]
                    if deployment.state.get('vm_id'):
                        from runner import execute_guest
                        logs=execute_guest(runner.hypervisor.guest(project,owner,deployment.state['vm_id']),
                            {'action':'logs','body':{'request_id':uuid.uuid4().hex}})
                        diagnostic += '\nGuest service logs:\n'+logs.get('output','')[-8000:]
                    for key,value in deployment.state.items():
                        if ('password' in key or 'token' in key) and isinstance(value,str):diagnostic=diagnostic.replace(value,'[redacted]')
                    print('Isolated test release diagnostics: '+diagnostic,flush=True)
            raise
        finally:
            if runner:
                for meta in runner.hypervisor.list(project,owner):
                    if meta['state']=='destroyed':continue
                    if runner.hypervisor.alive(meta):runner.hypervisor.stop(meta,True)
                    runner.hypervisor.perform(project,owner,'vm/destroy',{'vm_id':meta['vm_id'],'delete_data':True,'confirm_data_loss':True})
            for server in servers:server.shutdown();server.server_close()
            for process in reversed(processes):process.terminate();process.wait(timeout=10)


if __name__=='__main__':main()
