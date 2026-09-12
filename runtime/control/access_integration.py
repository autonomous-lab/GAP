"""Two real isolated nodes, local SMTP sink, shared authority, no production data."""
import base64
import email
import json
import os
from pathlib import Path
import queue
import re
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
import uuid

sys.path.insert(0, str(Path(os.environ.get('GAP_TEST_REPO', Path(__file__).resolve().parents[2]))/'scripts'))
from test_admin_http import SMTP, port
from access import Access
from authority import Authority
from service import Application, Server


@unittest.skipUnless(os.environ.get('GAP_TEST_BINARY'), 'GAP_TEST_BINARY required')
class FleetIdentityIntegration(unittest.TestCase):
    def test_verified_connection_signed_project_access_and_restart(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            smtp=socketserver.ThreadingTCPServer(('127.0.0.1',0),SMTP)
            smtp.messages=queue.Queue()
            threading.Thread(target=smtp.serve_forever,daemon=True).start()
            self.addCleanup(smtp.server_close);self.addCleanup(smtp.shutdown)
            authority=Authority(root/'control.sqlite','test-operator')
            access=Access(authority,b'k'*32)
            control=Server(('127.0.0.1',0),Application(authority,'o'*64,{'node-one':'w'*64,'node-two':'x'*64},
                access=access,identity_nodes={'node-one':'i'*64,'node-two':'j'*64}))
            threading.Thread(target=control.serve_forever,daemon=True).start()
            self.addCleanup(control.server_close);self.addCleanup(control.shutdown)
            control_url='http://127.0.0.1:'+str(control.server_port)
            def request(base,path,token=None,body=None,method=None):
                req=urllib.request.Request(base+path,method=method,
                    data=json.dumps(body).encode() if body is not None else None,
                    headers={'Content-Type':'application/json',**({'Authorization':'Bearer '+token} if token else {})})
                try:response=urllib.request.urlopen(req,timeout=10)
                except urllib.error.HTTPError as e:response=e
                with response:return response.status,json.loads(response.read())
            nodes=[]
            for node,credential in [('node-one','i'*64),('node-two','j'*64)]:
                folder=root/node;folder.mkdir()
                base='http://127.0.0.1:'+str(port())
                env=dict(PATH=os.environ['PATH'],GAP_ADDR=base.removeprefix('http://'),GAP_STORAGE='sqlite',
                    GAP_SQLITE_PATH=str(folder/'node.sqlite'),GAP_CLOUD_ROOT=str(folder/'projects'),GAP_WORKERS='4',
                    GAP_MASTER_KEY='c'*64,GAP_PUBLIC_URL='https://client.test',GAP_EMAIL_VERIFICATION_REQUIRED='1',
                    GAP_REGISTRATION_DB=str(folder/'registration.sqlite'),GAP_SMTP_HOST='127.0.0.1',
                    GAP_SMTP_PORT=str(smtp.server_address[1]),GAP_SMTP_FROM='test@example.com',
                    GAP_FLEET_ACCESS_ENABLED='1',GAP_FLEET_OPERATOR_ID='test-operator',GAP_FLEET_NODE_ID=node,
                    GAP_FLEET_PUBLIC_KEY=access.public_key,GAP_FLEET_IDENTITY_URL=control_url+'/identity',
                    GAP_FLEET_IDENTITY_TOKEN=credential)
                log=(folder/'node.log').open('w');self.addCleanup(log.close)
                def start(env=env,folder=folder,log=log,base=base):
                    proc=subprocess.Popen([os.environ['GAP_TEST_BINARY']],env=env,cwd=folder,stdout=log,stderr=log)
                    def stop():
                        if proc.poll() is None:proc.terminate();proc.wait(timeout=10)
                    self.addCleanup(stop)
                    for _ in range(200):
                        try:
                            if request(base,'/health')[0]==200:return proc
                        except OSError:pass
                        if proc.poll() is not None:self.fail('isolated node startup failed')
                        time.sleep(.05)
                    self.fail('isolated node startup timeout')
                proc=start()
                legacy=node=='node-one'
                if legacy:
                    # Create an old identity while verification is disabled,
                    # then enable it without replacing the existing DID/token.
                    proc.terminate();proc.wait(timeout=10);env['GAP_EMAIL_VERIFICATION_REQUIRED']='0';proc=start()
                    status,identity=request(base,'/v1/identity',body={});self.assertEqual(status,200)
                    _,other_identity=request(base,'/v1/identity',body={})
                    status,project=request(base,'/v1/cloud/projects',identity['token'],{});self.assertEqual(status,200)
                    proc.terminate();proc.wait(timeout=10);env['GAP_EMAIL_VERIFICATION_REQUIRED']='1';proc=start()
                    self.assertFalse(request(base,'/v1/identity/email',identity['token'])[1]['email_verified'])
                    self.assertEqual(request(base,'/v1/fleet/connect',identity['token'],{'project_id':project['project_id'],'request_id':'before-proof'})[0],403)
                endpoint='/v1/identity/email' if legacy else '/v1/identity'
                status,challenge=request(base,endpoint,identity['token'] if legacy else None,body={'email':'same-owner@example.com'})
                self.assertEqual(status,202,challenge)
                message=email.message_from_bytes(smtp.messages.get(timeout=5))
                text=''.join(p.get_payload(decode=True).decode() for p in message.walk() if p.get_content_type()=='text/plain')
                code=re.search(r'\b([0-9]{6})\b',text).group(1)
                proof={'challenge_id':challenge['challenge_id'],'code':code}
                if legacy:
                    self.assertIn(identity['did'],text)
                    self.assertEqual(request(base,'/v1/identity/email/verify',other_identity['token'],proof)[0],400)
                    self.assertEqual(request(base,'/v1/identity/verify',body=proof)[0],400)
                    status,linked_identity=request(base,'/v1/identity/email/verify',identity['token'],proof)
                    self.assertEqual(status,200,linked_identity);self.assertEqual(linked_identity['did'],identity['did'])
                    self.assertEqual(request(base,'/v1/identity/email/verify',identity['token'],proof)[0],409)
                    self.assertEqual(request(base,'/v1/cloud/projects',identity['token'])[1]['projects'][0]['project_id'],project['project_id'])
                else:
                    status,identity=request(base,'/v1/identity/verify',body=proof)
                    self.assertEqual(status,201,identity)
                    status,project=request(base,'/v1/cloud/projects',identity['token'],{})
                    self.assertEqual(status,200,project)
                status,linked=request(base,'/v1/fleet/connect',identity['token'],{'project_id':project['project_id'],'request_id':uuid.uuid4().hex})
                self.assertEqual(status,200,linked)
                nodes.append(dict(base=base,identity=identity,project=project['project_id'],linked=linked,proc=proc,start=start,env=env))
            one,two=nodes
            self.assertEqual(one['linked']['customer_id'],two['linked']['customer_id'])
            for current,other in [(one,two),(two,one)]:
                control_token=current['linked']['credential']['token']
                self.assertEqual(request(control_url,'/v1/project-token',control_token,{'project_id':other['project']})[0],403)
                status,result=request(control_url,'/v1/project-token',control_token,{'project_id':current['project']})
                self.assertEqual(status,200,result)
                token=result['token'];path='/v1/cloud/projects/'+current['project']+'/kv/fleet-test'
                self.assertEqual(request(current['base'],path,token,{'value_base64':base64.b64encode(b'preserved').decode()},'PUT')[0],200)
                self.assertEqual(request(current['base'],path,token)[1]['value_base64'],base64.b64encode(b'preserved').decode())
                self.assertNotEqual(request(other['base'],path,token)[0],200)
                self.assertNotEqual(request(current['base'],path.replace(current['project'],other['project']),token)[0],200)
                self.assertNotEqual(request(current['base'],'/v1/cloud/projects',token,{})[0],200)
                self.assertNotEqual(request(current['base'],'/v1/fleet/connect',token,{'project_id':current['project'],'request_id':'x'})[0],200)
                current['proc'].terminate();current['proc'].wait(timeout=10)
                current['proc']=current['start']()
                self.assertEqual(request(current['base'],path,token)[0],200)
                # Signature stays valid but past expiry must fail on the node.
                old_clock=authority.clock
                authority.clock=lambda:time.time()-400
                expired=access.issue(authority.authenticate(control_token),current['project'])['token']
                authority.clock=old_clock
                self.assertEqual(request(current['base'],path,expired)[0],401)
                self.assertEqual(request(control_url,'/v1/logout',control_token,{})[0],200)
                self.assertEqual(request(control_url,'/v1/project-token',control_token,{'project_id':current['project']})[0],401)
                current['proc'].terminate();current['proc'].wait(timeout=10)
                current['env']['GAP_FLEET_ACCESS_ENABLED']='0'
                current['proc']=current['start']()
                self.assertEqual(request(current['base'],path,token)[0],401)
                self.assertEqual(request(current['base'],path,current['identity']['token'])[0],200)
            self.assertEqual(authority.wallet(one['linked']['customer_id'])['balance_microcredits'],0)


if __name__=='__main__':unittest.main()
