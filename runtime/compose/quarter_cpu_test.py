"""Explicit disposable KVM validation: Free network, CFS bandwidth and resume."""
import json, os, subprocess, time, urllib.request, uuid
from pathlib import Path
from microvm import MicroVMs
from runner import execute_guest


def main():
    if os.environ.get('GAP_TEST_QUARTER_CPU') != '1': raise SystemExit('explicit opt-in required')
    state=os.environ.get('GAP_TEST_VM_STATE','/test-vms')
    m=MicroVMs({'state_dir':state,'image_dir':'/images','cpu_quota_socket':'/run/gap-cpu/quota.sock','diagnostic_serial':True},execute_guest)
    m.approval_provider=lambda project,owner:{'quota':{'max_vms':1,'vcpus':.5,'memory_mib':512},
        'tier':'free','network_restricted':True,'always_on_allowed':False}
    project='prj_'+uuid.uuid4().hex[:24]; owner='did:gap:'+uuid.uuid4().hex*2
    key=Path(state)/('rsa-test-'+uuid.uuid4().hex)
    subprocess.run(['ssh-keygen','-q','-t','rsa','-b','2048','-N','','-f',str(key)],check=True)
    meta=None
    def ssh(command):
        return subprocess.run(['ssh','-F','/dev/null','-o','BatchMode=yes','-o','IdentitiesOnly=yes','-o','ConnectTimeout=10','-o','StrictHostKeyChecking=yes','-o','HostKeyAlias='+meta['vm_id'],'-o','UserKnownHostsFile='+str(m.folder(meta)/'known_hosts'),'-i',str(key),'-p',str(meta['ssh_port']),'root@127.0.0.1',command],capture_output=True,text=True,timeout=60)
    try:
        m.perform(project,owner,'vm/create',{'request_id':uuid.uuid4().hex,'vcpus':.25,'memory_mib':512,
            'disk_gib':2,'ports':[8000],'ssh_keys':[Path(str(key)+'.pub').read_text().strip()]})
        meta=m.read(project,owner)
        deadline=time.monotonic()+180
        while time.monotonic()<deadline:
            result=ssh('id -u')
            if result.returncode==0: break
            time.sleep(1)
        else: raise AssertionError('RSA login failed: '+result.stderr)
        assert result.stdout.strip()=='0'; print('RSA login OK',flush=True)
        assert ssh('wget -T 3 -qO- http://1.1.1.1/').returncode!=0
        assert ssh('nohup python3 -m http.server 8000 >/tmp/http.log 2>&1 </dev/null &').returncode==0
        worker_port=meta['ports'][0]['worker_port']
        for _ in range(30):
            try:
                with urllib.request.urlopen(f'http://127.0.0.1:{worker_port}/',timeout=2) as response:
                    if response.status==200:break
            except Exception:time.sleep(1)
        else:raise AssertionError('reverse proxy host forward failed')
        print('Free egress blocked; reverse proxy forward OK',flush=True)
        assert ssh('nohup timeout 12 yes >/dev/null 2>&1 </dev/null &').returncode==0
        pid=m.children[meta['vm_id']].pid
        def ticks():
            fields=Path(f'/proc/{pid}/stat').read_text().split();return int(fields[13])+int(fields[14])
        a=ticks(); start=time.monotonic();time.sleep(8);ratio=(ticks()-a)/os.sysconf('SC_CLK_TCK')/(time.monotonic()-start)
        assert .15<ratio<.34,ratio; print('Busy CPU fraction',ratio,flush=True)
        assert ssh('killall yes 2>/dev/null || true; echo preserved > /root/resume-test').returncode==0
        m.hibernate(meta); assert not m.alive(meta)
        m.resume(meta)
        assert ssh('cat /root/resume-test').stdout.strip()=='preserved'
        assert meta['vcpus']==.25; print('Quarter CPU snapshot resume + RSA OK',flush=True)
    finally:
        meta=m.read(project,owner)
        if meta and meta['state']!='destroyed':
            if m.alive(meta):m.perform(project,owner,'vm/stop',{'request_id':uuid.uuid4().hex,'vm_id':meta['vm_id'],'force':True})
            m.perform(project,owner,'vm/destroy',{'request_id':uuid.uuid4().hex,'vm_id':meta['vm_id'],'delete_data':True,'confirm_data_loss':True})


if __name__=='__main__':main()
