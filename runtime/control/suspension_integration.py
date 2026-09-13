"""Two real isolated GAP nodes; policy RPC adapters, no production identities or email."""
import json, os, subprocess, tempfile, threading, time, unittest, urllib.request, urllib.error
from pathlib import Path
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from authority import Authority
from access import Access
from service import Application
import suspension

@unittest.skipUnless(os.environ.get('GAP_TEST_BINARY'),'GAP_TEST_BINARY required')
class FleetSuspensionIntegration(unittest.TestCase):
    def test_two_nodes_suspend_restore_and_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);a=Authority(root/'control.sqlite','test-op');access=Access(a,b'k'*32)
            app=Application(a,'o'*64,{'one':'1'*64,'two':'2'*64},access=access)
            customer=a.create_customer('operator','customer','Isolated suspension test')['customer_id']
            available=[True];nodes=[]
            def request(base,path,token=None,body=None,method=None):
                req=urllib.request.Request(base+path,method=method,data=None if body is None else json.dumps(body).encode(),
                    headers={'Content-Type':'application/json',**({'Authorization':'Bearer '+token} if token else {})})
                try:r=urllib.request.urlopen(req,timeout=5)
                except urllib.error.HTTPError as e:r=e
                with r:return r.status,json.loads(r.read())
            def handler(credential):
                class Worker(BaseHTTPRequestHandler):
                    def log_message(self,*args):pass
                    def do_POST(self):
                        body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                        assert self.headers['Authorization']=='Bearer '+'r'*64
                        assert body['action']=='admin/customer-policy'
                        result=app.handle('POST','/node',credential,{'action':'suspension-snapshot'}) if available[0] else {'error':{'code':'unavailable'}}
                        raw=json.dumps(result).encode();self.send_response(200 if available[0] else 503)
                        self.send_header('Content-Length',str(len(raw)));self.end_headers();self.wfile.write(raw)
                return Worker
            for index,node in enumerate(('one','two'),1):
                folder=root/node;folder.mkdir();approvals=folder/'approvals.json';approvals.write_text('{"agents":[]}')
                worker=ThreadingHTTPServer(('127.0.0.1',0),handler(str(index)*64));threading.Thread(target=worker.serve_forever,daemon=True).start()
                self.addCleanup(worker.server_close);self.addCleanup(worker.shutdown)
                import socket
                with socket.socket() as sock:sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
                base='http://127.0.0.1:'+str(port)
                env=dict(PATH=os.environ['PATH'],GAP_ADDR='127.0.0.1:'+str(port),GAP_STORAGE='sqlite',
                    GAP_SQLITE_PATH=str(folder/'node.sqlite'),GAP_CLOUD_ROOT=str(folder/'projects'),GAP_WORKERS='4',
                    GAP_MASTER_KEY='c'*64,GAP_FLEET_ACCESS_ENABLED='1',GAP_FLEET_OPERATOR_ID='test-op',
                    GAP_FLEET_NODE_ID=node,GAP_FLEET_PUBLIC_KEY=access.public_key,
                    GAP_FLEET_IDENTITY_URL='http://127.0.0.1:1/identity',GAP_FLEET_IDENTITY_TOKEN='i'*64,
                    GAP_FLEET_POLICY_ENABLED='1',GAP_COMPOSE_ENABLED='1',GAP_COMPOSE_APPROVALS_FILE=str(approvals),
                    GAP_COMPOSE_RUNNER_URL='http://127.0.0.1:'+str(worker.server_port),GAP_COMPOSE_RUNNER_TOKEN='r'*64)
                log=(folder/'node.log').open('w');self.addCleanup(log.close)
                proc=subprocess.Popen([os.environ['GAP_TEST_BINARY']],env=env,cwd=folder,stdout=log,stderr=log)
                def stop(p=proc):
                    if p.poll() is None:p.terminate();p.wait(timeout=10)
                self.addCleanup(stop)
                for _ in range(100):
                    try:
                        if request(base,'/health')[0]==200:break
                    except OSError:pass
                    if proc.poll() is not None:self.fail('isolated node startup failed')
                    time.sleep(.1)
                time.sleep(.3)
                status,identity=request(base,'/v1/identity',body={});self.assertEqual(status,200,identity)
                status,project=request(base,'/v1/cloud/projects',identity['token'],{});self.assertEqual(status,200,project)
                a.attach_principal('operator','agent-'+node,customer,'agent',identity['did'])
                a.attach_project('operator','project-'+node,customer,project['project_id'],node,identity['did'])
                cap=access.issue(dict(customer=customer,agent=None),project['project_id'])['token']
                path='/v1/cloud/projects/'+project['project_id']+'/kv/witness'
                self.assertEqual(request(base,path,identity['token'],{'value_base64':'cHJlc2VydmVk'},'PUT')[0],200)
                nodes.append((base,path,identity['token'],cap))
            body=dict(customer_id=customer,active=True,expected_revision=0,reason='Isolated fleet suspension validation')
            app.handle('POST','/operator','o'*64,dict(body,action='suspension-set',request_id='suspend'))
            def wait_status(allowed):
                deadline=time.monotonic()+15
                while time.monotonic()<deadline:
                    states=[request(base,path,token)[0]==200 for base,path,token,_ in nodes]
                    if all(s==allowed for s in states):return
                    time.sleep(.2)
                self.fail('policy did not converge on both nodes')
            wait_status(False)
            for base,path,token,cap in nodes:self.assertNotEqual(request(base,path,cap)[0],200)
            app.handle('POST','/operator','o'*64,dict(body,active=False,expected_revision=1,action='suspension-set',request_id='restore'))
            wait_status(True)
            for base,path,token,_ in nodes:self.assertEqual(request(base,path,token)[1]['value_base64'],'cHJlc2VydmVk')
            available[0]=False;wait_status(False)
            available[0]=True;wait_status(True)
            print('PASS: both real nodes deny local and previously signed tokens; restore preserves data; expired policy fails closed.',flush=True)

if __name__=='__main__':unittest.main()
