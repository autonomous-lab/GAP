"""Explicit disposable KVM validation: RSA login, CFS bandwidth, snapshot resume."""
import json, os, subprocess, time, uuid
from pathlib import Path
from microvm import MicroVMs
from runner import execute_guest


def main():
    if os.environ.get('GAP_TEST_QUARTER_CPU') != '1': raise SystemExit('explicit opt-in required')
    m=MicroVMs({'state_dir':'/test-vms','image_dir':'/images','cpu_quota_socket':'/run/gap-cpu/quota.sock','diagnostic_serial':True},execute_guest)
    project='prj_'+uuid.uuid4().hex[:24]; owner='did:gap:'+uuid.uuid4().hex*2
    key=Path('/test-vms/rsa-test-'+uuid.uuid4().hex)
    subprocess.run(['ssh-keygen','-q','-t','rsa','-b','2048','-N','','-f',str(key)],check=True)
    meta=None
    def ssh(command):
        return subprocess.run(['ssh','-F','/dev/null','-o','BatchMode=yes','-o','IdentitiesOnly=yes','-o','ConnectTimeout=3','-o','StrictHostKeyChecking=yes','-o','HostKeyAlias='+meta['vm_id'],'-o','UserKnownHostsFile='+str(m.folder(meta)/'known_hosts'),'-i',str(key),'-p',str(meta['ssh_port']),'root@127.0.0.1',command],capture_output=True,text=True,timeout=60)
    try:
        m.perform(project,owner,'vm/create',{'request_id':uuid.uuid4().hex,'vcpus':.25,'memory_mib':512,'disk_gib':2,'ssh_keys':[Path(str(key)+'.pub').read_text().strip()]})
        meta=m.read(project,owner)
        deadline=time.monotonic()+180
        while time.monotonic()<deadline:
            result=ssh('id -u')
            if result.returncode==0: break
            time.sleep(1)
        else: raise AssertionError('RSA login failed: '+result.stderr)
        assert result.stdout.strip()=='0'; print('RSA login OK',flush=True)
        assert ssh('nohup timeout 12 yes >/dev/null 2>&1 </dev/null &').returncode==0
        pid=m.children[meta['vm_id']].pid
        def ticks():
            fields=Path(f'/proc/{pid}/stat').read_text().split();return int(fields[13])+int(fields[14])
        a=ticks(); start=time.monotonic();time.sleep(8);ratio=(ticks()-a)/os.sysconf('SC_CLK_TCK')/(time.monotonic()-start)
        assert .15<ratio<.34,ratio; print('Busy CPU fraction',ratio,flush=True)
        assert ssh('killall yes; echo preserved > /root/resume-test').returncode==0
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
