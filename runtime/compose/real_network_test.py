"""Disposable real KVM + public TCP/UDP + SSH/SFTP + hot key rotation test."""
import base64
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import uuid

from microvm import MicroVMs
from runner import execute_guest


def main():
    if os.environ.get('GAP_VM_TEST_ALLOW_CREATE') != '1':
        raise SystemExit('explicit disposable VM opt-in required')
    root = Path('/validation')
    config = {'state_dir': '/test-vms', 'image_dir': '/images', 'diagnostic_serial': True,
              'public_network': {'hostname': 'sites.gap.geta.team', 'first_port': 24000, 'last_port': 24009}}
    manager = MicroVMs(config, execute_guest)
    project, owner = 'prj_'+uuid.uuid4().hex[:24], 'did:gap:'+uuid.uuid4().hex*2
    key = root / ('network-key-'+uuid.uuid4().hex)
    key2 = root / ('network-key-'+uuid.uuid4().hex)
    for item in (key, key2):
        subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(item)], check=True)
    def call(action, **body):
        return manager.perform(project, owner, 'vm/'+action, dict(body, request_id=uuid.uuid4().hex))['vm']
    def ready():
        deadline = time.monotonic()+120
        while time.monotonic()<deadline:
            try:
                if execute_guest(manager.guest(project, owner), {'action':'vm_probe','body':{}}, timeout=10).get('ok'):
                    return
            except Exception:
                pass
            time.sleep(1)
        raise AssertionError('guest readiness timeout')
    def ssh(command, identity=key, success=True):
        args = ['ssh', '-F', '/dev/null', '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes',
                '-o', 'StrictHostKeyChecking=yes', '-o', 'ConnectTimeout=5', '-o', 'HostKeyAlias='+vm['vm_id'],
                '-o', 'UserKnownHostsFile='+str(manager.folder(meta)/'known_hosts'),
                '-i', str(identity), '-p', str(port), 'root@127.0.0.1', command]
        result = subprocess.run(args, capture_output=True, timeout=20)
        if success:
            assert result.returncode == 0, result.stderr.decode()
        else:
            assert result.returncode != 0
        return result.stdout
    def net(mappings):
        return manager.network.perform(project,owner,'ports',dict(request_id=uuid.uuid4().hex,vm_id=vm['vm_id'],mappings=mappings))
    def exchange(public_port, protocol):
        with socket.socket(type=socket.SOCK_STREAM if protocol=='tcp' else socket.SOCK_DGRAM) as sock:
            sock.settimeout(5)
            sock.connect(('127.0.0.1',public_port))
            sock.send(b'gap-network-test')
            assert sock.recv(100)==b'gap-network-test'
    try:
        vm=call('create',ssh_keys=[Path(str(key)+'.pub').read_text().strip()],disk_gib=4)
        meta=manager.read(project,owner)
        ports=meta['public_ports'];port=ports[0]
        ready()
        mappings=[dict(slot=1,guest_port=22,protocol='tcp'),dict(slot=2,guest_port=7000,protocol='both'),
                  dict(slot=3,guest_port=7001,protocol='tcp'),dict(slot=4,guest_port=7001,protocol='udp'),
                  dict(slot=5,guest_port=7002,protocol='tcp')]
        net(mappings)
        assert ssh('id -u').strip()==b'0'
        source='''import socket,threading,time
def serve(kind,port):
 s=socket.socket(type=kind);s.bind(('0.0.0.0',port))
 if kind==socket.SOCK_STREAM:
  s.listen()
  while True:
   c,a=s.accept();c.sendall(c.recv(100));c.close()
 else:
  while True:
   b,a=s.recvfrom(100);s.sendto(b,a)
for port in (7000,7001,7002):
 for kind in (socket.SOCK_STREAM,socket.SOCK_DGRAM):
  threading.Thread(target=serve,args=(kind,port),daemon=True).start()
while True:time.sleep(10)
'''
        encoded=base64.b64encode(source.encode()).decode()
        ssh("echo '"+encoded+"' | base64 -d > /tmp/echo.py; nohup python3 /tmp/echo.py >/tmp/echo.log 2>&1 </dev/null &")
        time.sleep(1)
        for index, protocol in [(1,'tcp'),(1,'udp'),(2,'tcp'),(3,'udp'),(4,'tcp')]:
            print('probe', ports[index], protocol, flush=True)
            exchange(ports[index],protocol)
        # SFTP subsystem used by modern scp.
        target=root/'network-upload.txt';target.write_text('sftp-persistent')
        subprocess.run(['scp','-F','/dev/null','-o','BatchMode=yes','-o','StrictHostKeyChecking=yes',
                        '-o','HostKeyAlias='+vm['vm_id'],'-o','UserKnownHostsFile='+str(manager.folder(meta)/'known_hosts'),
                        '-i',str(key),'-P',str(port),str(target),'root@127.0.0.1:/root/network-upload.txt'],check=True,timeout=15)
        before_pid=(manager.folder(meta)/'qemu.pid').read_text()
        manager.network.perform(project,owner,'ssh',dict(request_id=uuid.uuid4().hex,vm_id=vm['vm_id'],authorized_keys=[Path(str(key2)+'.pub').read_text().strip()]))
        ssh('true',success=False)
        assert ssh('cat /root/network-upload.txt',key2)==b'sftp-persistent'
        assert before_pid==(manager.folder(meta)/'qemu.pid').read_text()
        (root/'network-ready.json').write_text(json.dumps({'ports':ports,'vm_id':vm['vm_id']}))
        print('SSH + SFTP + TCP/UDP + HOT KEY ROTATION OK; awaiting external probe',flush=True)
        deadline=time.monotonic()+120
        while not (root/'network-external-ok').exists() and time.monotonic()<deadline:
            time.sleep(1)
        assert (root/'network-external-ok').exists(), 'external probe required'
        net([])
        with socket.socket() as sock:
            sock.settimeout(2)
            assert sock.connect_ex(('127.0.0.1',port))!=0
        net(mappings)
        call('stop',vm_id=vm['vm_id'])
        manager=MicroVMs(config,execute_guest)
        call('start',vm_id=vm['vm_id']);ready()
        assert manager.read(project,owner)['public_ports']==ports
        ssh('true',success=False)
        assert ssh('cat /root/network-upload.txt',key2)==b'sftp-persistent'
        print('EXTERNAL PROBE + DISABLE/ENABLE + STOP/START + KEY/PORT/DISK PERSISTENCE OK',flush=True)
    finally:
        meta=manager.read(project,owner)
        if meta and meta['state']!='destroyed':
            if manager.alive(meta):
                call('stop',vm_id=meta['vm_id'],force=True)
            call('destroy',vm_id=meta['vm_id'],delete_data=True,confirm_data_loss=True)
        for item in (key,key2):
            item.unlink(missing_ok=True);Path(str(item)+'.pub').unlink(missing_ok=True)
        for name in ('network-ready.json','network-external-ok','network-upload.txt'):
            (root/name).unlink(missing_ok=True)
        print('DISPOSABLE VM AND TEST KEYS REMOVED',flush=True)


if __name__=='__main__':
    main()
