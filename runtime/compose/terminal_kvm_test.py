"""Opt-in disposable KVM test. Never reads the production VM catalog."""
import base64
import os
from pathlib import Path
import tempfile
import time
import uuid
from types import SimpleNamespace
from microvm import MicroVMs
from runner import Runner, execute_guest, ssh_command
from terminal import Session


def main():
    if os.environ.get('GAP_TEST_TERMINAL')!='1':raise SystemExit('explicit opt-in required')
    project='prj_'+uuid.uuid4().hex[:24];owner='did:gap:'+uuid.uuid4().hex*2
    with tempfile.TemporaryDirectory(prefix='gap-terminal-kvm-') as folder:
        m=MicroVMs({'state_dir':folder,'image_dir':'/images'},execute_guest)
        meta=None;s=None
        try:
            m.perform(project,owner,'vm/create',{'request_id':uuid.uuid4().hex,'vcpus':1,'memory_mib':512,'disk_gib':2})
            meta=m.read(project,owner)
            runner=Runner.__new__(Runner);runner.hypervisor=m
            runner.runtime=SimpleNamespace(check_policy=lambda meta:True,check_credit=lambda meta:None,touch=lambda meta:None)
            deadline=time.monotonic()+60
            while True:
                try:
                    if execute_guest(m.guest(project,owner,meta['vm_id']),{'action':'vm_probe','body':{}},timeout=5).get('ok'):break
                except Exception:
                    if time.monotonic()>=deadline:raise
                if time.monotonic()>=deadline:raise AssertionError('guest readiness timeout')
                time.sleep(1)
            result=runner.dispatch_job({'project':project,'owner':owner},{},{'action':'terminal/prepare','body':{'vm_id':meta['vm_id']}})
            assert result['ok']
            guest=m.guest(project,owner,meta['vm_id']);guest['ssh_key']=str(m.folder(meta)/'terminal_key')
            command=ssh_command(guest);command[command.index('-T')]='-tt';command[command.index('ServerAliveInterval=10')]='ServerAliveInterval=0';command[-1]='exec /bin/sh -l'
            if os.environ.get('GAP_TEST_TTYD')=='1':
                from test_ttyd_terminal import acceptance
                acceptance(command)
                return
            s=Session(command,(project,owner,meta['vm_id']),80,24)
            seq=0;cursor=0;output=b''
            def send(data=b'',cols=80,rows=24):
                nonlocal seq,cursor,output
                seq+=1
                r=s.exchange({'seq':seq,'cursor':cursor,'input':base64.b64encode(data).decode(),'cols':cols,'rows':rows})
                cursor=r['cursor'];output+=base64.b64decode(r['output']);return r
            def expect(marker,timeout=20):
                deadline=time.monotonic()+timeout
                while marker not in output and time.monotonic()<deadline:
                    time.sleep(.1);r=send()
                    if r['closed']:raise AssertionError(output.decode(errors='replace'))
                assert marker in output,output.decode(errors='replace')
            # Wait for shell prompt, then verify command output and PTY dimensions.
            expect(b'# ')
            send(b"printf '\\107\\101\\120\\055\\120\\124\\131\\055'; id -u; stty size\n",100,32)
            expect(b'GAP-PTY-0');expect(b'32 100')
            print('Real guest SSH PTY: root shell and resize OK',flush=True)
            output=b'';send(b'sleep 60\n');time.sleep(.3);send(b'\x03');send(b"printf '\\103\\124\\122\\114\\055\\103\\055\\117\\113\\n'\n")
            expect(b'CTRL-C-OK');print('Ctrl-C interrupts foreground guest command OK',flush=True)
            s.close();assert s.process.poll() is not None
            # The restricted deployment credential must still work separately.
            assert execute_guest(m.guest(project,owner,meta['vm_id']),{'action':'runtime_environment','body':{'variables':m.environment(meta)}},timeout=15)['ok']
            print('Close reaped SSH process; deployment credential remains restricted and functional',flush=True)
        finally:
            if s:s.close()
            meta=m.read(project,owner)
            if meta and meta['state']!='destroyed':
                if m.alive(meta):m.perform(project,owner,'vm/stop',{'request_id':uuid.uuid4().hex,'vm_id':meta['vm_id'],'force':True})
                m.perform(project,owner,'vm/destroy',{'request_id':uuid.uuid4().hex,'vm_id':meta['vm_id'],'delete_data':True,'confirm_data_loss':True})

if __name__=='__main__':main()
