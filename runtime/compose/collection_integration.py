"""Opt-in real KVM collection isolation, wake routing and retained-volume billing.
Run only in an isolated container with disposable GAP_VM_TEST_STATE_DIR.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import unittest
import urllib.request
import uuid
import integration_test as fixture
import serverless_test as serverless


@unittest.skipUnless(os.environ.get('GAP_TEST_VM_COLLECTION')=='1' and os.environ.get('GAP_TEST_BINARY'), 'isolated collection test opt-in required')
class CollectionIntegration(fixture.Integration):
    def exercise_managed(self,request,prefix,token,other_token,runner,project,owner,approval,root):
        base=prefix[:-len('/stack')]; manager=runner.hypervisor; runtime=runner.runtime
        approval.write_text(json.dumps({'agents':[owner],'quotas':{owner:{'vcpus':4,'memory_mib':4096,'max_vms':2}}}))
        tariff={'version':'test-collection','vcpu_hour':10000,'gib_ram_hour':10000,'gb_disk_month':100000,'gb_in':10000,'gb_out':10000}
        runtime.ledger.set_pricing('enforced',tariff)
        runtime.ledger.topup(project,owner,100000000,'isolated-test-only')
        key=root/'collection-ssh-key'
        subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(key)],check=True)
        def op(method,path,body=None,error=None):
            body={'request_id':uuid.uuid4().hex,**(body or {})}
            status,job=request(method,base+path,token,body)
            self.assertIn(status,(200,202),job)
            deadline=time.monotonic()+120
            while time.monotonic()<deadline:
                _,result=request('GET',base+'/vm/jobs/'+job['job_id'],token)
                if result['status'] not in ('queued','running'):
                    self.assertEqual(result['status'],'failed' if error else 'succeeded',result)
                    if error:self.assertEqual(result['result']['error'],error)
                    return result.get('result')
                time.sleep(.1)
            self.fail('collection job timed out')
        def ssh(meta,command,input=None,public=False):
            folder=manager.folder(meta)
            return subprocess.run(['ssh','-F','/dev/null','-o','BatchMode=yes','-o','IdentitiesOnly=yes',
                '-o','ConnectTimeout=3','-o','StrictHostKeyChecking=yes','-o','HostKeyAlias='+meta['vm_id'],
                '-o','UserKnownHostsFile='+str(folder/'known_hosts'),'-i',str(key),
                '-p',str(meta['public_ports'][2] if public else meta['ssh_port']),'root@127.0.0.1',command],
                input=input,text=True,capture_output=True,check=True,timeout=30).stdout
        def get(url):
            try:
                with urllib.request.urlopen(url,timeout=90) as response:return json.load(response)
            except urllib.error.HTTPError as error:
                self.fail('application response: '+error.read().decode()+'; gateway '+str(getattr(runtime.gateway,'last_error_type',None)))
        try:
            metas=[];urls=[];baselines=[]
            for _ in range(2):
                vm=op('POST','/vms',{'ports':[8000],'start':True,'ssh_keys':[Path(str(key)+'.pub').read_text().strip()]})['vm']
                meta=manager.read(project,owner,vm['vm_id']);metas.append(meta)
                for attempt in range(100):
                    try:ssh(meta,'true');break
                    except subprocess.SubprocessError:time.sleep(.2)
                else:self.fail('SSH not ready')
                app=serverless.APP.replace('self.request.sendall(self.request.recv(65536))',"self.request.sendall(identity.encode()+b':'+self.request.recv(65536))").replace('sock.sendto(data,self.client_address)',"sock.sendto(identity.encode()+b':'+data,self.client_address)")
                ssh(meta,'rc-service docker stop >/dev/null 2>&1; cat > /root/collection-app.py',app)
                ssh(meta,'nohup python3 /root/collection-app.py >/tmp/collection-app.log 2>&1 </dev/null &')
                for attempt in range(30):
                    try:json.loads(ssh(meta,'wget -T 2 -qO- http://127.0.0.1:8000/'));break
                    except (subprocess.SubprocessError,ValueError):time.sleep(.2)
                else:self.fail('test app unavailable: '+ssh(meta,'cat /tmp/collection-app.log'))
                op('PUT','/vm/ingress',{'vm_id':vm['vm_id'],'enabled':True,'guest_port':8000})
                status,ingress=request('GET',base+'/vm/ingress?vm_id='+vm['vm_id'],token)
                self.assertEqual(status,200,ingress);urls.append(ingress['url']);baselines.append(get(urls[-1]))
                op('PUT','/vm/ports',{'vm_id':vm['vm_id'],'mappings':[{'slot':1,'guest_port':8001,'protocol':'tcp'},{'slot':2,'guest_port':8002,'protocol':'udp'},{'slot':3,'guest_port':22,'protocol':'tcp'}]})
            status,listed=request('GET',base+'/vms',token)
            self.assertEqual(status,200,listed);self.assertEqual(len(listed['vms']),2)
            self.assertEqual(request('GET',base+'/vms',other_token)[0],401)
            self.assertEqual(len(set(metas[0]['public_ports']) & set(metas[1]['public_ports'])),0)
            self.assertNotEqual(urls[0],urls[1]);self.assertNotEqual(baselines[0]['identity'],baselines[1]['identity'])
            op('POST','/vms',{'start':False},error='agent_quota_exceeded_max_vms')
            for meta in metas:op('POST','/vm/hibernate',{'vm_id':meta['vm_id']})
            self.assertEqual(get(urls[1]),baselines[1])
            self.assertEqual(manager.read(project,owner,metas[0]['vm_id'])['state'],'hibernated')
            op('POST','/vm/hibernate',{'vm_id':metas[1]['vm_id']})
            with socket.create_connection(('127.0.0.1',metas[0]['public_ports'][0]),timeout=90) as client:
                client.sendall(b'tcp-one');self.assertEqual(client.recv(100),baselines[0]['identity'].encode()+b':tcp-one')
            self.assertEqual(manager.read(project,owner,metas[1]['vm_id'])['state'],'hibernated')
            with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as client:
                client.settimeout(90);client.sendto(b'udp-two',('127.0.0.1',metas[1]['public_ports'][1]))
                self.assertEqual(client.recv(100),baselines[1]['identity'].encode()+b':udp-two')
            self.assertIn(metas[1]['vm_id'],ssh(metas[1],'cat /etc/gap/runtime.json',public=True))
            op('POST','/vm/stop',{'vm_id':metas[0]['vm_id']})
            op('DELETE','/vm',{'vm_id':metas[0]['vm_id']})
            self.assertEqual(get(urls[1]),baselines[1])
            replacement=op('POST','/vms',{'start':False})['vm']
            self.assertNotEqual(replacement['vm_id'],metas[0]['vm_id'])
            self.assertEqual(manager.read(project,owner,metas[0]['vm_id'])['state'],'destroyed')
            with runtime.lock(project):
                remaining=manager.list(project,owner)
                for meta in remaining:runtime.sample(meta)
                view=runtime.ledger.view(project,owner)
                self.assertEqual(sum(e['debited_microcredits'] for e in view['entries'] if e['kind']=='usage'),view['spent_microcredits'])
                self.assertEqual(view['balance_microcredits']+view['spent_microcredits'],100000000)
                # Simulate the configured project's expired balance in the test DB.
                with runtime.ledger.db() as db:db.execute('UPDATE accounts SET balance=0,exhausted_at=? WHERE project=?',(time.time()-3*86400-1,project))
                runtime.expire(remaining[0])
                self.assertTrue(all(m['state']=='destroyed' for m in manager.list(project,owner)))
                self.assertTrue(all(runtime.storage_bytes(m)==0 for m in manager.list(project,owner)))
                self.assertFalse(runtime.ledger.view(project,owner)['deletion_committed'])
            print('PASS: two VMs, selected API/HTTPS/TCP/UDP/SSH wake, independent routes, count quota, delete/recreate, ledger reconciliation, project retention deletion',flush=True)
        finally:
            runtime.closed=True
            with runtime.lock(project):
                for meta in manager.list(project,owner):
                    try:
                        if manager.alive(meta):manager.stop(meta,True)
                        if meta['state']!='destroyed':manager._perform(project,owner,'vm/destroy',{'vm_id':meta['vm_id'],'delete_data':True,'confirm_data_loss':True})
                    except Exception:pass

if __name__=='__main__':unittest.main()
