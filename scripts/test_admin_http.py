"""Isolated real-node admin auth tests. No message leaves the local SMTP sink."""
import email
import base64
import json
import os
from pathlib import Path
import queue
import re
import socket
import socketserver
import sqlite3
import subprocess
import sys
from types import SimpleNamespace
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.parse
import urllib.request


def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


class SMTP(socketserver.StreamRequestHandler):
    def handle(self):
        self.wfile.write(b'220 isolated test relay\r\n')
        while line := self.rfile.readline():
            verb = line.split(b' ', 1)[0].strip().upper()
            if verb == b'DATA':
                self.wfile.write(b'354 send body\r\n')
                parts = []
                while (part := self.rfile.readline()) not in (b'.\r\n', b''):
                    parts.append(part)
                self.server.messages.put(b''.join(parts))
                self.wfile.write(b'250 accepted locally\r\n')
            elif verb == b'QUIT':
                self.wfile.write(b'221 bye\r\n')
                return
            else:
                self.wfile.write(b'250 OK\r\n')


@unittest.skipUnless(os.environ.get('GAP_TEST_BINARY'), 'GAP_TEST_BINARY required')
class AdminHTTP(unittest.TestCase):
    def test_password_email_session_origin_csrf_and_inventory(self):
        with tempfile.TemporaryDirectory() as directory, socketserver.ThreadingTCPServer(('127.0.0.1', 0), SMTP) as smtp:
            smtp.messages = queue.Queue()
            threading.Thread(target=smtp.serve_forever, daemon=True).start()
            root = Path(directory)
            (root/'approvals.json').write_text('{"agents":[]}')
            runner_dir=Path(__file__).resolve().parent.parent/'runtime/compose'
            if not runner_dir.exists():runner_dir=Path('/opt/runner')
            sys.path.insert(0,str(runner_dir))
            from runner import Runner,handler_for
            from billing import Ledger
            runner=Runner.__new__(Runner);runner.token='b'*64;runner.operator_token='o'*64
            runner.runtime=SimpleNamespace(ledger=Ledger(root/'billing.sqlite'))
            worker=ThreadingHTTPServer(('127.0.0.1',0),handler_for(runner))
            threading.Thread(target=worker.serve_forever,daemon=True).start()
            dns_answers=[]
            class DNS(BaseHTTPRequestHandler):
                def do_GET(self):
                    self.send_response(200);self.end_headers();self.wfile.write(json.dumps({'Answer':[{'type':16,'data':v} for v in dns_answers]}).encode())
                def log_message(self,*args):pass
            dns=ThreadingHTTPServer(('127.0.0.1',0),DNS)
            threading.Thread(target=dns.serve_forever,daemon=True).start()
            self.addCleanup(dns.shutdown);self.addCleanup(dns.server_close)
            base = 'http://127.0.0.1:' + str(port())
            env = {'PATH': os.environ['PATH'], 'GAP_ADDR': base.removeprefix('http://'),
                   'GAP_STORAGE': 'sqlite', 'GAP_SQLITE_PATH': str(root/'node.sqlite'),
                   'GAP_VM_SESSIONS_DB': str(root/'vm-sessions.sqlite'), 'GAP_CLOUD_ROOT': str(root/'projects'), 'GAP_WORKERS': '4',
                   'GAP_VM_EDGE_TOKEN':'e'*64, 'GAP_CADDY_ASK_TOKEN':'f'*64, 'GAP_CUSTOM_DOMAIN_TARGET':'node.example.com', 'GAP_DNS_JSON_RESOLVER':'http://127.0.0.1:'+str(dns.server_port),
                   'GAP_MASTER_KEY': 'c'*64, 'GAP_PUBLIC_URL': 'https://client.test',
                   'GAP_CLOUD_ADMIN_ENABLED': '1', 'GAP_ADMIN_EMAILS': 'owner@example.com',
                   'GAP_ADMIN_ORIGIN': 'https://admin.test', 'GAP_ADMIN_DB': str(root/'admin.sqlite'),
                   'GAP_ADMIN_CHALLENGES_DB': str(root/'challenges.sqlite'),
                   'GAP_SMTP_HOST': '127.0.0.1', 'GAP_SMTP_PORT': str(smtp.server_address[1]),
                   'GAP_SMTP_FROM': 'test@example.com', 'GAP_COMPOSE_ENABLED': '1',
                   'GAP_COMPOSE_APPROVALS_FILE': str(root/'approvals.json'),
                   'GAP_COMPOSE_RUNNER_TOKEN': 'b'*64, 'GAP_COMPOSE_RUNNER_URL': 'http://127.0.0.1:'+str(worker.server_port)}
            with (root/'node.log').open('w') as log:
                proc = subprocess.Popen([os.environ['GAP_TEST_BINARY']], env=env, cwd=root, stdout=log, stderr=log)
                try:
                    def request(method, path, body=None, cookie=None, csrf=None, host='admin.test', origin='https://admin.test', bearer=None, basic=None, vm_csrf=None, extra=None):
                        headers = {'Host': host, 'Origin': origin, 'Content-Type': 'application/json'}
                        if bearer: headers['Authorization'] = 'Bearer '+bearer
                        if basic: headers['Authorization']='Basic '+base64.b64encode(basic.encode()).decode()
                        if cookie: headers['Cookie'] = cookie
                        if csrf: headers['X-CSRF-Token'] = csrf
                        if vm_csrf: headers['X-GAP-VM-Session']=vm_csrf
                        headers.update(extra or {})
                        req = urllib.request.Request(base+path, method=method, headers=headers,
                                                     data=json.dumps(body).encode() if body is not None else None)
                        try: response = urllib.request.urlopen(req, timeout=10)
                        except urllib.error.HTTPError as error: response = error
                        with response:
                            raw = response.read().decode()
                            try: value = json.loads(raw)
                            except ValueError: value = raw
                            return response.status, value, response.headers
                    for _ in range(200):
                        try:
                            if request('GET', '/health')[0] == 200: break
                        except OSError: pass
                        if proc.poll() is not None: self.fail('Isolated node failed: '+(root/'node.log').read_text())
                        time.sleep(.05)
                    def restart(admin_enabled):
                        nonlocal proc
                        proc.terminate();proc.wait(timeout=10)
                        proc=subprocess.Popen([os.environ['GAP_TEST_BINARY']],env={**env,'GAP_CLOUD_ADMIN_ENABLED':admin_enabled},cwd=root,stdout=log,stderr=log)
                        for _ in range(200):
                            try:
                                if request('GET','/health')[0]==200:return
                            except OSError:pass
                            if proc.poll() is not None:self.fail('Node restart failed: '+(root/'node.log').read_text())
                            time.sleep(.05)
                        self.fail('Node did not restart')
                    api = '/v1/admin/console/'
                    self.assertEqual(request('GET', '/admin')[0], 200)
                    self.assertEqual(request('GET', '/apps/tenant')[0], 404)
                    self.assertEqual(request('POST', '/v1/identity')[0], 404)
                    self.assertEqual(request('GET', api+'session')[0], 401)
                    self.assertEqual(request('POST', api+'login', {}, origin='https://client.test')[0], 403)
                    self.assertEqual(request('GET', api+'session', host='client.test')[0], 403)
                    credentials = {'email':'owner@example.com','password':'unique strong admin password'}
                    status, challenge, _ = request('POST', api+'login', credentials)
                    self.assertEqual(status, 202)
                    self.assertNotIn('code', challenge)
                    msg = email.message_from_bytes(smtp.messages.get(timeout=5))
                    body = msg.get_payload(decode=True).decode()
                    code = re.search(r'\b\d{6}\b', body).group()
                    status, session, headers = request('POST', api+'verify', {'challenge_id':challenge['challenge_id'],'code':code})
                    self.assertEqual(status, 200)
                    cookie = headers['Set-Cookie']
                    for flag in ['Secure', 'HttpOnly', 'SameSite=Strict', 'Path=/']:
                        self.assertIn(flag, cookie)
                    self.assertNotIn('Domain=', cookie)
                    cookie = cookie.split(';')[0]
                    self.assertEqual(request('POST', api+'verify', {'challenge_id':challenge['challenge_id'],'code':code})[0], 400)
                    self.assertEqual(request('GET', api+'session', cookie=cookie)[0], 200)
                    self.assertEqual(request('GET', api+'overview', cookie=cookie)[0], 200)
                    self.assertEqual(request('GET', api+'finance')[0], 401)
                    status, report, _=request('GET', api+'finance',cookie=cookie)
                    self.assertEqual(status,200)
                    self.assertTrue(report['available'])
                    self.assertIsNone(report['usage_margin_microdollars'])
                    self.assertEqual(request('GET',api+'finance?start=1&end=3600',cookie=cookie)[0],400)
                    self.assertEqual(request('GET', api+'agents', cookie=cookie)[1]['agents'], [])
                    _, agent, _ = request('POST', '/v1/identity', host='client.test')
                    _, project, _ = request('POST', '/v1/cloud/projects', {}, host='client.test', bearer=agent['token'])
                    project_id = project['project_id']
                    _, other_project, _=request('POST','/v1/cloud/projects',{},host='client.test',bearer=agent['token'])
                    status, detail, _=request('GET',api+'agents/'+urllib.parse.quote(agent['did'],safe=''),cookie=cookie)
                    self.assertEqual(status,200);self.assertEqual(detail['total'],2)
                    self.assertEqual({p['project_id'] for p in detail['projects']},{project_id,other_project['project_id']})
                    self.assertNotIn(agent['token'],json.dumps(detail))
                    route = '/v1/cloud/projects/'+project_id+'/access-requests'
                    status, access, _ = request('POST', route, {'request_id':'d'*32,'quota':{'vcpus':2,'memory_mib':4096,'max_vms':1},'always_on':False,'reason':'Test workload'}, host='client.test', bearer=agent['token'])
                    self.assertEqual(status, 202)
                    status, listed, _ = request('GET', api+'approvals', cookie=cookie)
                    self.assertEqual(status, 200)
                    self.assertEqual(listed['requests'][0]['id'], access['id'])
                    self.assertEqual(request('POST', api+'approvals/'+access['id'], {'approve':True}, cookie=cookie)[0], 403)
                    status, reviewed, _ = request('POST', api+'approvals/'+access['id'], {'approve':True}, cookie=cookie, csrf=session['csrf'])
                    self.assertEqual(status, 200)
                    self.assertEqual(reviewed['status'], 'approved')
                    self.assertIn(agent['did'], json.loads((root/'approvals.json').read_text())['agents'])
                    from unittest.mock import patch
                    vm_session='/v1/cloud/projects/'+project_id+'/browser-session'
                    with patch.object(runner,'rpc',return_value=(200,{'vms':[]})) as rpc_mock:
                        self.assertEqual(request('POST',vm_session,host='admin.test',origin='https://client.test',bearer=agent['token'])[0],403)
                        st,session_vm,headers_vm=request('POST',vm_session,host='admin.test',origin='https://admin.test',bearer=agent['token'])
                        self.assertEqual(st,201)
                        cookie_vm=headers_vm['Set-Cookie'].split(';')[0]
                        proc.terminate();proc.wait(timeout=10)
                        proc=subprocess.Popen([os.environ['GAP_TEST_BINARY']],env=env,cwd=root,stdout=log,stderr=log)
                        for _ in range(200):
                            try:
                                if request('GET','/health')[0]==200:break
                            except OSError:pass
                            time.sleep(.05)

                        self.assertIn('HttpOnly',headers_vm['Set-Cookie']);self.assertNotIn(agent['token'],headers_vm['Set-Cookie'])
                        vms='/v1/cloud/projects/'+project_id+'/vms'
                        self.assertEqual(request('POST',vm_session,host='admin.test',origin='https://admin.test',cookie=cookie_vm,vm_csrf=session_vm['csrf'])[0],401)
                        self.assertEqual(request('GET',vms,host='admin.test',cookie=cookie_vm)[0],401)
                        self.assertEqual(request('GET',vms,host='admin.test',cookie=cookie_vm,vm_csrf=session_vm['csrf'])[0],200)
                        metrics='/v1/cloud/projects/'+project_id+'/vm/metrics?vm_id=vm_'+'a'*32
                        self.assertEqual(request('GET',metrics,host='admin.test',cookie=cookie_vm)[0],401)
                        self.assertEqual(request('GET',metrics,host='admin.test',cookie=cookie_vm,vm_csrf=session_vm['csrf'])[0],200)
                        forwarded=rpc_mock.call_args.args[0]
                        self.assertEqual(forwarded['action'],'metrics')
                        self.assertEqual(forwarded['body'],{'vm_id':'vm_'+'a'*32})
                        self.assertEqual(forwarded['project_id'],project_id)
                        self.assertEqual(request('GET',metrics.replace(project_id,other_project['project_id']),host='admin.test',cookie=cookie_vm,vm_csrf=session_vm['csrf'])[0],401)
                        self.assertEqual(request('GET','/v1/cloud/projects/'+other_project['project_id']+'/vms',host='admin.test',cookie=cookie_vm,vm_csrf=session_vm['csrf'])[0],401)
                        self.assertEqual(request('DELETE',vm_session,host='admin.test',origin='https://admin.test',cookie=cookie_vm,vm_csrf=session_vm['csrf'])[0],200)
                        self.assertEqual(request('DELETE',vm_session,host='admin.test',origin='https://admin.test',cookie=cookie_vm)[0],200)
                        self.assertEqual(request('GET',vms,host='admin.test',cookie=cookie_vm,vm_csrf=session_vm['csrf'])[0],401)
                    # Mandatory VM visitor auth and verified custom-domain routing.
                    vm_id='vm_'+'a'*32
                    vm_base='/v1/cloud/projects/'+project_id+'/vm'
                    ingress={'vm_id':vm_id,'base_path':'/apps/'+project_id+'/','enabled':True,'routed':True}
                    def admission(path=None,basic=None,hostname='client.test',secret='e'*64,raw=None):
                        path=path or ingress['base_path']
                        headers={'X-GAP-Edge-Token':secret,'X-GAP-Original-Host':hostname,'X-GAP-Original-Path':path,'X-GAP-Original-URI':raw or path,'X-GAP-Client-IP':'192.0.2.4'}
                        if basic:headers['X-GAP-Original-Authorization']='Basic '+base64.b64encode(basic.encode()).decode()
                        return request('GET','/internal/vm-http-admission',extra=headers)
                    with patch.object(runner,'rpc',return_value=(200,ingress)) as mock:
                        self.assertEqual(request('GET',vm_base+'/http-access?vm_id='+vm_id,host='client.test')[0],401)
                        self.assertEqual(request('GET',vm_base+'/domains?vm_id='+vm_id,host='client.test',bearer='wrong')[0],401)
                        self.assertEqual(admission()[0],401)
                        self.assertEqual(admission(secret='wrong')[0],403)
                        self.assertEqual(admission('/health')[0],200)
                        mock.assert_not_called() # admission must not call back into the worker
                        self.assertEqual(request('PUT',vm_base+'/http-access',{'vm_id':vm_id,'username':'visitor','password':'too short'},host='client.test',bearer=agent['token'])[0],400)
                        credentials={'vm_id':vm_id,'username':'visitor','password':'isolated VM password'}
                        status,access,_=request('PUT',vm_base+'/http-access',credentials,host='client.test',bearer=agent['token'])
                        self.assertEqual(status,200,access);self.assertTrue(access['configured']);self.assertNotIn('password',access);self.assertNotIn('password_hash',access);self.assertTrue(access['password_recoverable'])
                        reveal=vm_base+'/http-access/reveal'
                        self.assertEqual(request('POST',reveal,{'vm_id':vm_id},host='client.test')[0],401)
                        self.assertEqual(request('POST',reveal,{'vm_id':vm_id},host='client.test',bearer='wrong')[0],401)
                        st,revealed,rh=request('POST',reveal,{'vm_id':vm_id},host='client.test',bearer=agent['token'])
                        self.assertEqual(st,200,revealed);self.assertEqual(revealed['password'],credentials['password']);self.assertIn('no-store',rh['Cache-Control'])
                        self.assertEqual(request('POST',reveal.replace(project_id,other_project['project_id']),{'vm_id':vm_id},host='client.test',bearer=agent['token'])[0],400)
                        _,owner_session,owner_headers=request('POST',vm_session,host='admin.test',bearer=agent['token'])
                        owner_cookie=owner_headers['Set-Cookie'].split(';')[0]
                        self.assertEqual(request('POST',reveal,{'vm_id':vm_id},cookie=owner_cookie)[0],401)
                        self.assertEqual(request('POST',reveal,{'vm_id':vm_id},cookie=owner_cookie,vm_csrf=owner_session['csrf'])[1]['password'],credentials['password'])
                        self.assertEqual(request('POST',reveal,{'vm_id':vm_id},cookie=owner_cookie,vm_csrf=owner_session['csrf'],origin='https://client.test')[0],403)
                        self.assertEqual(admission(basic='visitor:wrong')[0],401)
                        status,_,headers=admission(basic='visitor:isolated VM password')
                        self.assertEqual(status,200);self.assertEqual(headers['X-GAP-VM-URI'],ingress['base_path']);self.assertEqual(headers['X-GAP-VM-Strip-Auth'],'1')
                        self.assertEqual(admission(path=ingress['base_path']+'../other',basic='visitor:isolated VM password')[0],403)
                        self.assertEqual(headers['X-GAP-VM-Identity'],vm_id)
                        payload={'vm_id':vm_id,'hostname':'app.customer.test'}
                        st,domain,_=request('POST',vm_base+'/domains',payload,host='client.test',bearer=agent['token'])
                        self.assertEqual(st,200,domain);self.assertEqual(domain['domain']['status'],'pending_dns')
                        self.assertEqual(domain['domain']['vm_id'],vm_id)
                        self.assertEqual(request('POST',vm_base+'/domains',{**payload,'vm_id':'vm_'+'b'*32},host='client.test',bearer=agent['token'])[0],400)
                        self.assertEqual(request('POST','/v1/cloud/projects/'+project_id+'/site/domains/'+payload['hostname']+'/verify',{},host='client.test',bearer=agent['token'])[0],400)
                        self.assertEqual(request('POST','/v1/cloud/projects/'+project_id+'/site/domains',{'hostname':payload['hostname'],'access':'public'},host='client.test',bearer=agent['token'])[0],400)
                        verify=vm_base+'/domains/'+payload['hostname']+'/verify'
                        self.assertEqual(request('POST',verify,{'vm_id':vm_id},host='client.test',bearer=agent['token'])[0],400)
                        dns_answers.append(domain['domain']['verification_value'])
                        self.assertEqual(request('POST',verify,{'vm_id':vm_id},host='client.test',bearer=agent['token'])[0],200)
                        st,_,headers=admission('/hello world',hostname=payload['hostname'],raw='/hello%20world?q=1')
                        self.assertEqual(st,200);self.assertEqual(headers['X-GAP-VM-URI'],ingress['base_path']+'hello%20world?q=1');self.assertNotIn('X-GAP-VM-Strip-Auth',headers)
                        self.assertEqual(admission()[0],401) # custom domain never unlocks the shared origin
                        self.assertEqual(request('GET','/internal/tls/ask?token='+('f'*64)+'&domain='+payload['hostname'],host='client.test')[0],200)
                        restart('1')
                        self.assertEqual(request('POST',reveal,{'vm_id':vm_id},host='client.test',bearer=agent['token'])[1]['password'],credentials['password'])
                        self.assertEqual(admission(basic='visitor:isolated VM password')[0],200)
                        self.assertEqual(admission('/hello',hostname=payload['hostname'])[0],200)
                        with sqlite3.connect(root/'node.sqlite') as db:
                            stored=db.execute("SELECT value FROM node_state WHERE scope='cloud_vm_http' AND key=?",(vm_id,)).fetchone()[0]
                        self.assertNotIn(credentials['password'],stored)
                        original_record=json.loads(stored)
                        self.assertTrue(original_record['password_sealed'].startswith('enc:v1:'))
                        for altered in ({k:v for k,v in original_record.items() if k!='password_sealed'},
                                        {**original_record,'password_sealed':'plaintext is forbidden'},
                                        {**original_record,'username':'swapped-username'}):
                            with sqlite3.connect(root/'node.sqlite') as db:
                                db.execute("UPDATE node_state SET value=? WHERE scope='cloud_vm_http' AND key=?",(json.dumps(altered),vm_id))
                            restart('1')
                            self.assertEqual(request('POST',reveal,{'vm_id':vm_id},host='client.test',bearer=agent['token'])[0],400)
                        with sqlite3.connect(root/'node.sqlite') as db:
                            db.execute("UPDATE node_state SET value=? WHERE scope='cloud_vm_http' AND key=?",(stored,vm_id))
                        restart('1')
                        self.assertEqual(request('POST',reveal,{'vm_id':vm_id},host='client.test',bearer=agent['token'])[1]['password'],credentials['password'])

                        st,_,_=request('DELETE',vm_base+'/domains/'+payload['hostname'],{'vm_id':vm_id},host='client.test',bearer=agent['token']);self.assertEqual(st,200)
                        self.assertNotIn('X-GAP-VM-URI',admission('/hello',hostname=payload['hostname'])[2])
                    self.assertNotIn(agent['token'], json.dumps(request('GET', api+'agents', cookie=cookie)[1]))
                    self.assertEqual(request('GET', api+'projects/'+project_id, cookie=cookie)[0], 200)
                    site='/v1/cloud/projects/'+project_id+'/site'
                    self.assertEqual(request('PUT',site,{'enabled':True,'entrypoint':'index.html','auth':{'mode':'basic','username':'visitor','password':'isolated site password'}},host='client.test',bearer=agent['token'])[0],200)
                    _, release, _=request('POST',site+'/versions',{},host='client.test',bearer=agent['token'])
                    version=str(release['version'])
                    self.assertEqual(request('PUT',site+'/versions/'+version+'/files/index.html',{'content_base64':base64.b64encode(b'<h1>Private test site</h1>').decode()},host='client.test',bearer=agent['token'])[0],200)
                    self.assertEqual(request('POST',site+'/versions/'+version+'/activate',{},host='client.test',bearer=agent['token'])[0],200)
                    public='/sites/'+project_id+'/'
                    self.assertEqual(request('GET',public,host='client.test',basic='visitor:isolated site password')[0],200)
                    project_suspend=api+'projects/'+project_id+'/suspension'
                    project_decision={'active':True,'expected_generation':0,'reason':'Isolated project review'}
                    self.assertEqual(request('POST',project_suspend,project_decision,cookie=cookie)[0],403)
                    self.assertEqual(request('POST',project_suspend,project_decision,cookie=cookie,csrf=session['csrf'])[0],200)
                    self.assertEqual(request('GET',public,host='client.test',basic='visitor:isolated site password')[0],404)
                    self.assertEqual(request('GET','/v1/cloud/projects/'+other_project['project_id']+'/kv/check',host='client.test',bearer=agent['token'])[0],200)
                    self.assertFalse(request('GET',api+'projects/'+project_id,cookie=cookie)[1]['project']['execution_allowed'])
                    self.assertEqual(request('POST',project_suspend,project_decision,cookie=cookie,csrf=session['csrf'])[0],409)
                    suspend=api+'agents/'+agent['did']+'/suspension'
                    decision={'active':True,'expected_generation':0,'reason':'Isolated abuse test'}
                    self.assertEqual(request('POST',suspend,decision,cookie=cookie)[0],403)
                    status, recorded, _=request('POST',suspend,decision,cookie=cookie,csrf=session['csrf'])
                    self.assertEqual(status,200);self.assertTrue(recorded['decision']['active'])
                    restart('0')
                    self.assertEqual(request('GET','/v1/cloud/projects',host='client.test',bearer=agent['token'])[0],401)
                    self.assertEqual(request('GET',public,host='client.test',basic='visitor:isolated site password')[0],404)
                    policy=request('POST','/internal/workload-policy',{'project_id':project_id},host='client.test',bearer='b'*64)[1]
                    self.assertFalse(policy['allowed']);self.assertTrue(policy['suspended'])
                    restart('1')
                    self.assertEqual(request('GET',api+'session',cookie=cookie)[0],200)
                    self.assertEqual(request('POST',suspend,decision,cookie=cookie,csrf=session['csrf'])[0],409)
                    self.assertEqual(request('POST',suspend,{'active':False,'expected_generation':1,'reason':'Resolved in isolated test'},cookie=cookie,csrf=session['csrf'])[0],200)
                    self.assertEqual(request('GET',public,host='client.test',basic='visitor:isolated site password')[0],404)
                    self.assertEqual(request('POST',project_suspend,{'active':False,'expected_generation':1,'reason':'Project review resolved'},cookie=cookie,csrf=session['csrf'])[0],200)
                    policy=request('POST','/internal/workload-policy',{'project_id':project_id},host='client.test',bearer='b'*64)[1]
                    self.assertEqual(policy['generation'],4);self.assertTrue(policy['allowed'])
                    self.assertEqual(request('GET',public,host='client.test',basic='visitor:isolated site password')[0],200)

                    self.assertEqual(request('POST', api+'logout', {}, cookie=cookie)[0], 403)
                    self.assertEqual(request('POST', api+'logout', {}, cookie=cookie, csrf=session['csrf'], origin='https://client.test')[0], 403)
                    self.assertEqual(request('POST', api+'login', {**credentials,'password':'different wrong password'})[0], 401)
                    self.assertEqual(request('POST', api+'logout', {}, cookie=cookie, csrf=session['csrf'])[0], 200)
                    self.assertEqual(request('GET', api+'session', cookie=cookie)[0], 401)
                finally:
                    proc.terminate()
                    proc.wait(timeout=10)
                    smtp.shutdown()
                    worker.shutdown();worker.server_close()


if __name__ == '__main__':
    unittest.main()
