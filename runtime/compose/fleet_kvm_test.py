"""Isolated real-KVM acceptance for controller loss under a long deployment lock."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import threading
import time
import unittest
import uuid

sys.path.insert(0,os.environ.get('GAP_TEST_CONTROL',str(Path(__file__).resolve().parents[1]/'control')))
from authority import Authority
from service import Application,Server
import integration_test
from billing import Ledger,RETENTION_SECONDS

APP='''import socketserver,uuid
identity=uuid.uuid4().hex.encode()
class Echo(socketserver.BaseRequestHandler):
 def handle(self):
  while True:
   data=self.request.recv(100)
   if not data:return
   self.request.sendall(identity+b":"+data)
socketserver.ThreadingTCPServer(("0.0.0.0",8001),Echo).serve_forever()
'''


@unittest.skipUnless(os.environ.get('GAP_TEST_FLEET')=='1','explicit isolated fleet/KVM opt-in required')
class FleetAcceptance(integration_test.Integration):
    test_public_end_to_end_control_plane=None

    def configure_fleet_fixture(self,config,root,project,owner):
        self.authority_path=root/'control.sqlite'
        self.control_port=integration_test.free_port()
        self.control_node_token='n'*64
        self.start_controller()
        a=self.authority
        self.customer=a.create_customer('operator','customer','Isolated KVM')['customer_id']
        a.attach_principal('operator','agent',self.customer,'agent',owner)
        a.attach_project('operator','project',self.customer,project,'test-node',owner)
        a.topup('operator','test-funding',self.customer,1000000,'promotional')
        token=root/'control-node.token';token.write_text(self.control_node_token);token.chmod(0o600)
        config['fleet_billing']=dict(url=f'http://127.0.0.1:{self.control_port}',token_file=str(token),
             operator_id='test-operator',node_id='test-node',projects=[project],target_microcredits=100000,lease_seconds=15)
        Path(config['state_dir']).mkdir(mode=0o700,parents=True,exist_ok=True)
        Ledger(Path(config['state_dir'])/'microvm-credits.sqlite').set_pricing('enforced',
            {'version':'fleet-kvm-test','vcpu_hour':10000,'gib_ram_hour':10000,'gb_disk_month':100000,'gb_in':10000,'gb_out':10000})

    def start_controller(self):
        self.authority=Authority(self.authority_path,'test-operator')
        self.control=Server(('127.0.0.1',self.control_port),Application(self.authority,'o'*64,
                     {'test-node':self.control_node_token},allow_reservations=True))
        self.control_thread=threading.Thread(target=self.control.serve_forever,daemon=True)
        self.control_thread.start()

    def stop_controller(self):
        if self.control:
            self.control.shutdown();self.control.server_close();self.control_thread.join(5)
            self.control=None

    def exercise_managed(self,request,prefix,token,other_token,runner,project,owner,approval,root):
        prefix=prefix[:-len('/stack')]+'/vm'
        runtime=runner.runtime;manager=runner.hypervisor;meta=None
        key=root/'owner-key'
        subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(key)],check=True)
        def op(method,path,body):
            status,job=request(method,prefix+path,token,dict(request_id=uuid.uuid4().hex,**body))
            self.assertEqual(status,202,job)
            deadline=time.monotonic()+120
            while time.monotonic()<deadline:
                _,result=request('GET',prefix+'/jobs/'+job['job_id'],token)
                if result['status'] not in ('queued','running'):
                    self.assertEqual(result['status'],'succeeded',result)
                    return result['result']
                time.sleep(.1)
            self.fail('fleet KVM job timeout')
        def ssh(command,input=None):
            return subprocess.run(['ssh','-F','/dev/null','-o','BatchMode=yes','-o','IdentitiesOnly=yes',
                '-o','ConnectTimeout=2','-o','StrictHostKeyChecking=yes','-o','UserKnownHostsFile='+str(manager.folder(meta)/'known_hosts'),
                '-o','HostKeyAlias='+meta['vm_id'],'-i',str(key),'-p',str(meta['ssh_port']),
                'root@127.0.0.1',command],input=input,text=True,capture_output=True,check=True,timeout=10).stdout
        try:
            op('POST','',{'ports':[8001],'ssh_keys':[Path(str(key)+'.pub').read_text().strip()]})
            meta=manager.read(project,owner)
            for _ in range(45):
                try:ssh('true');break
                except subprocess.SubprocessError:time.sleep(.5)
            else:self.fail('SSH readiness timeout')
            ssh('cat > /root/fleet-test.py',APP)
            ssh('printf preserved > /root/fleet-preserved; nohup python3 /root/fleet-test.py >/tmp/fleet-test.log 2>&1 </dev/null &')
            op('PUT','/ports',{'vm_id':meta['vm_id'],'mappings':[{'slot':1,'guest_port':8001,'protocol':'tcp'}]})
            meta=manager.read(project,owner)
            with socket.create_connection(('127.0.0.1',meta['public_ports'][0]),timeout=20) as tcp:
                tcp.sendall(b'probe');baseline=tcp.recv(100)
                self.assertTrue(baseline.endswith(b':probe'))
                with runtime.lock(project):
                    before=self.authority.wallet(self.customer)['spent_microcredits']
                    time.sleep(7)
                    self.assertGreater(self.authority.wallet(self.customer)['spent_microcredits'],before)
                    self.assertTrue(runtime.ledger.lease_allowed(project))
                    self.stop_controller()
                    deadline=time.monotonic()+20
                    while time.monotonic()<deadline:
                        if manager.public(meta)['state']=='paused':break
                        time.sleep(.1)
                    self.assertEqual(manager.public(meta)['state'],'paused')
                    self.assertEqual(tcp.recv(1),b'')
                    self.assertIsNone(runtime.state(meta)['running_since'])
            deadline=time.monotonic()+60
            while manager.read(project,owner)['state']!='hibernated' and time.monotonic()<deadline:time.sleep(.1)
            self.assertEqual(manager.read(project,owner)['state'],'hibernated',runtime.last_error)
            self.assertTrue(manager.folder(meta).exists())
            view=runtime.ledger.view(project,owner)
            self.assertFalse(view['execution_allowed']);self.assertIsNone(view['delete_after'])
            with runtime.ledger.db() as db:
                db.execute('UPDATE accounts SET exhausted_at=? WHERE project=?',(time.time()-RETENTION_SECONDS-1,project))
            self.assertFalse(runtime.ledger.claim_expired(project,owner,meta['vm_id']))
            runtime.tick()
            self.assertTrue(manager.folder(meta).exists())
            self.start_controller()
            deadline=time.monotonic()+20
            while not runtime.ledger.lease_allowed(project) and time.monotonic()<deadline:
                runtime.ledger.sync(project,owner,True);time.sleep(.1)
            self.assertTrue(runtime.ledger.lease_allowed(project))
            with socket.create_connection(('127.0.0.1',meta['public_ports'][0]),timeout=60) as tcp:
                tcp.sendall(b'probe');self.assertEqual(tcp.recv(100),baseline)
            meta=manager.read(project,owner)
            self.assertEqual(ssh('cat /root/fleet-preserved'),'preserved')
            runtime.closed=True;time.sleep(1.5)
            manager.hibernate(meta);runtime.sample(meta)
            runtime.ledger.sync(project,owner,True)
            self.assertEqual(self.authority.wallet(self.customer)['spent_microcredits'],runtime.ledger.view(project,owner)['spent_microcredits'])
            print('PASS: real HTTP reservations, metering under job lock, controller loss, TCP closure, CPU preemption, retained snapshot, exact settlement and preserved-memory resume',flush=True)
        finally:
            runtime.closed=True
            if meta:
                current=manager.read(project,owner)
                if manager.alive(current):manager.stop(current,True)
                if current['state']!='destroyed':manager.perform(project,owner,'vm/destroy',{'vm_id':current['vm_id'],'delete_data':True,'confirm_data_loss':True})
            self.stop_controller()


if __name__=='__main__':unittest.main()
