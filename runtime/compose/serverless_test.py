"""Opt-in, isolated real KVM acceptance: wake protocols, metering and retention.
Uses the same disposable node/worker/Caddy fixture as integration_test.py.
"""
import base64
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import unittest
import urllib.request
import uuid
from integration_test import Integration
from billing import RETENTION_SECONDS

APP='''import base64,hashlib,json,os,socket,socketserver,threading,time,uuid
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
identity=uuid.uuid4().hex
class HTTP(BaseHTTPRequestHandler):
 def log_message(self,*a): pass
 def do_GET(self):
  if self.headers.get('Upgrade','').lower()=='websocket':
   key=self.headers['Sec-WebSocket-Key']+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11'
   self.send_response(101);self.send_header('Upgrade','websocket');self.send_header('Connection','Upgrade');self.send_header('Sec-WebSocket-Accept',base64.b64encode(hashlib.sha1(key.encode()).digest()).decode());self.end_headers()
   frame=self.rfile.read(2); n=frame[1]&127;mask=self.rfile.read(4); data=self.rfile.read(n);data=bytes(x^mask[i%4] for i,x in enumerate(data));self.wfile.write(bytes([129,len(data)])+data);self.wfile.flush();return
  if self.path=='/slow': time.sleep(2)
  data=json.dumps({'identity':identity,'boot':open('/proc/sys/kernel/random/boot_id').read().strip()}).encode()
  self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
 def do_POST(self):
  data=self.rfile.read(int(self.headers.get('Content-Length','0')))
  self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
class TCP(socketserver.BaseRequestHandler):
 def handle(self): self.request.sendall(self.request.recv(65536))
class UDP(socketserver.BaseRequestHandler):
 def handle(self): data,sock=self.request;sock.sendto(data,self.client_address)
for server in [socketserver.ThreadingTCPServer(('0.0.0.0',8001),TCP),socketserver.ThreadingUDPServer(('0.0.0.0',8002),UDP)]: threading.Thread(target=server.serve_forever,daemon=True).start()
def outgoing():
 s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)
 while True:
  if os.path.exists('/tmp/send-outgoing'): s.sendto(b'outgoing-does-not-prevent-idle'*20,('10.0.2.2',9))
  time.sleep(.1)
threading.Thread(target=outgoing,daemon=True).start()
ThreadingHTTPServer(('0.0.0.0',8000),HTTP).serve_forever()
'''


@unittest.skipUnless(os.environ.get('GAP_TEST_SERVERLESS')=='1','explicit serverless test opt-in required')
class Serverless(Integration):
    def exercise_managed(self,request,prefix,token,other_token,runner,project,owner,approval,root):
        prefix=prefix[:-len('/stack')]+'/vm'
        manager=runner.hypervisor; runtime=runner.runtime
        def op(method,path,body=None,error=None):
            payload={'request_id':uuid.uuid4().hex,**(body or {})}
            status,job=request(method,prefix+path,token,payload)
            self.assertEqual(status,202,job)
            deadline=time.monotonic()+120
            while time.monotonic()<deadline:
                _,result=request('GET',prefix+'/jobs/'+job['job_id'],token)
                if result['status'] not in ('queued','running'):
                    self.assertEqual(result['status'],'failed' if error else 'succeeded',result)
                    if error: self.assertEqual(result['result']['error'],error)
                    return result['result']
                time.sleep(.1)
            self.fail('job timeout')
        meta=None
        key=root/'owner-key'
        subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(key)],check=True)
        def ssh(command,input=None,public=False):
            return subprocess.run(['ssh','-F','/dev/null','-o','BatchMode=yes','-o','IdentitiesOnly=yes',
                '-o','ConnectTimeout=2','-o','StrictHostKeyChecking=yes','-o','UserKnownHostsFile='+str(manager.folder(meta)/'known_hosts'),
                '-o','HostKeyAlias='+meta['vm_id'],'-i',str(key),'-p',str(meta['public_ports'][2] if public else meta['ssh_port']),
                'root@127.0.0.1',command],input=input,text=True,capture_output=True,check=True,timeout=20).stdout
        try:
            vm=op('POST','',{'ports':[8000],'ssh_keys':[Path(str(key)+'.pub').read_text().strip()]})['vm']
            meta=manager.read(project,owner); identity={'vm_id':vm['vm_id']}
            for _ in range(100):
                try: ssh('true');break
                except subprocess.SubprocessError: time.sleep(.2)
            else:self.fail('SSH unavailable')
            ssh('rc-service docker stop >/dev/null 2>&1; cat > /root/test-app.py',APP)
            ssh('nohup python3 /root/test-app.py >/tmp/test-app.log 2>&1 </dev/null &')
            for _ in range(30):
                try: json.loads(ssh('wget -T 2 -qO- http://127.0.0.1:8000/'));break
                except (subprocess.SubprocessError,ValueError):time.sleep(.1)
            else:self.fail('test HTTP application not ready')
            op('PUT','/ingress',{**identity,'enabled':True,'guest_port':8000})
            url=request('GET',prefix+'/ingress',token)[1]['url']
            def get(path=''):
                try:
                    with urllib.request.urlopen(url+path,timeout=90) as response:return json.load(response)
                except urllib.error.HTTPError as error:
                    if error.code!=402:
                        print('WAKE ERROR',error.read().decode(),runtime.last_error,manager.read(project,owner)['state'],getattr(runtime.gateway,'last_error_type',None),flush=True)
                    raise
            baseline=get()
            # A request in progress must survive an artificially expired idle timer.
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                slow=pool.submit(get,'slow')
                deadline=time.monotonic()+5
                while not runtime.state(meta)['active_http'] and time.monotonic()<deadline:time.sleep(.01)
                with runtime.lock(project):runtime.state(meta)['last_incoming']=time.time()-901
                runtime.tick()
                self.assertEqual(manager.read(project,owner)['state'],'running')
                self.assertEqual(slow.result(),baseline)
            self.assertEqual(request('GET',prefix+'/credits',other_token)[0],401)
            op('PUT','/runtime',{**identity,'mode':'always_on'},error='always_on_not_approved')
            approval.write_text(json.dumps({'agents':[owner],'always_on_agents':[owner]}))
            op('PUT','/runtime',{**identity,'mode':'always_on'})
            op('PUT','/runtime',{**identity,'mode':'serverless'})
            # Automatic idle timer, accelerated by changing only the test state.
            with runtime.lock(project): runtime.state(meta)['last_incoming']=time.time()-901
            deadline=time.monotonic()+60
            while time.monotonic()<deadline:
                if manager.read(project,owner)['state']=='hibernated':break
                time.sleep(.1)
            self.assertEqual(manager.read(project,owner)['state'],'hibernated',runtime.last_error)
            self.assertFalse(manager.alive(meta))
            started=time.monotonic()
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                responses=list(pool.map(lambda _:get(),range(8)))
            self.assertTrue(all(x==baseline for x in responses))
            print('AUTO HIBERNATE + 8 HTTP WAKE REQUESTS + PRESERVED STATE OK',round(time.monotonic()-started,3),flush=True)
            cycles=int(os.environ.get('GAP_TEST_WAKE_CYCLES','3'))
            if not 1<=cycles<=100:raise ValueError('test wake cycles must be 1..100')
            wake_times=[]
            for repeat in range(cycles):
                op('POST','/hibernate',identity)
                started=time.monotonic()
                with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                    self.assertTrue(all(x==baseline for x in pool.map(lambda _:get(),range(8))))
                if (repeat+1)%10==0:print('CONCURRENT WAKE CYCLES COMPLETED',repeat+1,flush=True)
                wake_times.append(time.monotonic()-started)
            print('REPEATED CONCURRENT WAKE CYCLES OK',cycles,'min/max seconds',round(min(wake_times),3),round(max(wake_times),3),flush=True)
            op('POST','/hibernate',identity)
            body=b'POST-body-must-arrive-once'
            with urllib.request.urlopen(urllib.request.Request(url,data=body),timeout=90) as response:self.assertEqual(response.read(),body)
            op('POST','/hibernate',identity)
            from urllib.parse import urlsplit
            address=urlsplit(url)
            with socket.create_connection((address.hostname,address.port or 80),timeout=90) as ws:
                ws.sendall(('GET '+address.path+'socket HTTP/1.1\r\nHost: '+address.netloc+'\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n').encode())
                stream=ws.makefile('rb'); self.assertIn(b'101',stream.readline())
                while stream.readline()!=b'\r\n':pass
                ws.sendall(b'\x81\x84abcd'+bytes(c^b'abcd'[i%4] for i,c in enumerate(b'ping')))
                self.assertEqual(stream.read(6),b'\x81\x04ping');stream.close()
            mappings=[{'slot':1,'guest_port':8001,'protocol':'tcp'},{'slot':2,'guest_port':8002,'protocol':'udp'},{'slot':3,'guest_port':22,'protocol':'tcp'}]
            op('PUT','/ports',{**identity,'mappings':mappings})
            meta=manager.read(project,owner)
            op('POST','/hibernate',identity)
            with socket.create_connection(('127.0.0.1',meta['public_ports'][0]),timeout=90) as tcp:
                tcp.sendall(b'tcp-wake');self.assertEqual(tcp.recv(100),b'tcp-wake')
            op('POST','/hibernate',identity)
            with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as udp:
                udp.settimeout(90);udp.sendto(b'udp-first-packet',('127.0.0.1',meta['public_ports'][1]))
                self.assertEqual(udp.recv(100),b'udp-first-packet')
            op('POST','/hibernate',identity)
            self.assertEqual(ssh('printf ssh-wake',public=True),'ssh-wake')
            print('POST + WEBSOCKET + TCP + UDP + SSH WAKE OK',flush=True)
            ssh('touch /tmp/send-outgoing')
            time.sleep(1.2)
            with runtime.lock(project):
                state=runtime.state(meta); last=state['last_incoming']
            time.sleep(1.2)
            self.assertEqual(runtime.state(meta)['last_incoming'],last)
            meter=manager.meters[meta['vm_id']]
            self.assertGreater(meter.values()[1],0)
            with runtime.lock(project): runtime.state(meta)['last_incoming']=time.time()-901
            runtime.tick()
            self.assertEqual(manager.read(project,owner)['state'],'hibernated')
            self.assertEqual(get(),baseline)
            # Real prepaid debits and retention, isolated test tariff only.
            price={'version':'acceptance-only','vcpu_hour':3600000,'gib_ram_hour':3600000,
                   'gib_disk_hour':3600,'gib_in':100,'gib_out':100}
            runner.operator({'action':'topup','project_id':project,'owner_did':owner,'amount_microcredits':1000000,'request_id':'test-topup'})
            runner.operator({'action':'set-pricing','mode':'enforced','tariff':price})
            time.sleep(1.2);runtime.sample(manager.read(project,owner));runtime.tick()
            account=request('GET',prefix+'/credits',token)[1]
            self.assertGreater(account['spent_microcredits'],0)
            self.assertGreater(account['balance_microcredits'],0)
            with runtime.ledger.db() as db:db.execute('UPDATE accounts SET balance=0 WHERE project=?',(project,))
            runtime.sample(manager.read(project,owner));runtime.tick()
            self.assertFalse(manager.alive(meta))
            account=request('GET',prefix+'/credits',token)[1]
            self.assertIsNotNone(account['delete_after'])
            with self.assertRaises(urllib.error.HTTPError) as error:get()
            self.assertEqual(error.exception.code,402)
            runner.operator({'action':'topup','project_id':project,'owner_did':owner,'amount_microcredits':1000000,'request_id':'restore-access'})
            self.assertIsNone(request('GET',prefix+'/credits',token)[1]['delete_after'])
            self.assertEqual(get(),baseline)
            with runtime.lock(project):
                with runtime.ledger.db() as db:db.execute('UPDATE accounts SET balance=0,exhausted_at=? WHERE project=?',(time.time()-RETENTION_SECONDS-1,project))
                runtime.tick()
            self.assertEqual(manager.read(project,owner)['state'],'destroyed')
            self.assertFalse(manager.folder(meta).exists())
            print('HOST NETWORK METER + INBOUND ONLY IDLE + REAL DEBIT + RECHARGE + 72H DELETE OK',flush=True)
        finally:
            runtime.closed=True
            if meta:
                with runtime.lock(project):
                    runtime.gateway.withdraw(project)
                    meta=manager.read(project,owner)
                    if manager.alive(meta):manager.stop(meta,True)
                    if meta['state']!='destroyed':manager._perform(project,owner,'vm/destroy',{'vm_id':meta['vm_id'],'delete_data':True,'confirm_data_loss':True})


if __name__=='__main__':unittest.main()
