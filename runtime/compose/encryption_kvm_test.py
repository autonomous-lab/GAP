"""Explicit isolated real-KVM disk/snapshot/transfer acceptance; no live catalog."""
import json,os,subprocess,tempfile,time,uuid,hashlib,shutil
from pathlib import Path
from microvm import MicroVMs,VMError
from disk_crypto import DiskCrypto

def main():
 if os.environ.get('GAP_TEST_ENCRYPTION')!='1':raise SystemExit('explicit opt-in required')
 with tempfile.TemporaryDirectory(prefix='crypt-') as d:
  root=Path(d);keyfile=root/'keys';keyfile.write_text(json.dumps({'active':'test','keys':{'test':os.urandom(32).hex()}}));keyfile.chmod(0o600)
  key=root/'ssh';subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(key)],check=True)
  config={'state_dir':str(root/'v'),'image_dir':'/images','disk_keyring':str(keyfile),'diagnostic_serial':True}
  m=MicroVMs(config,None);project='prj_'+uuid.uuid4().hex[:24];owner='did:gap:'+'b'*64;meta=None
  try:
   m.perform(project,owner,'vm/create',{'request_id':uuid.uuid4().hex,'vcpus':1,'memory_mib':256,'disk_gib':4,'ssh_keys':[key.with_suffix('.pub').read_text().strip()]})
   meta=m.read(project,owner)
   def ssh(cmd):
    r=subprocess.run(['ssh','-i',str(key),'-p',str(meta['ssh_port']),'-o','StrictHostKeyChecking=no','-o','UserKnownHostsFile=/dev/null','-o','ConnectTimeout=2','root@127.0.0.1',cmd],capture_output=True,text=True,timeout=10)
    if r.returncode:raise RuntimeError('guest SSH unavailable')
    return r.stdout.strip()
   for _ in range(45):
    try:boot=ssh('cat /proc/sys/kernel/random/boot_id');break
    except Exception:time.sleep(1)
   else:raise RuntimeError('guest boot timeout')
   marker='GAP-ENCRYPTION-'+uuid.uuid4().hex
   ssh(f'echo {marker} > /root/proof; echo {marker} > /dev/shm/proof; sync')
   m.hibernate(meta)
   disk=m.folder(meta)/'disk.qcow2'
   info=json.loads(subprocess.check_output(['qemu-img','info','--output=json',str(disk)]))
   assert info.get('encrypted') is True,info
   with disk.open('rb') as f:
    while chunk:=f.read(1024*1024):assert marker.encode() not in chunk
   # Missing key must preserve the hibernated state.
   saved=keyfile.read_text();keyfile.write_text(json.dumps({'active':'other','keys':{'other':'ab'*32}}))
   try:m.resume(meta);raise AssertionError('missing key accepted')
   except VMError:pass
   assert meta['state']=='hibernated'
   keyfile.write_text(saved)
   memory=m.folder(meta)/'memory.enc';original=memory.read_bytes()
   damaged=bytearray(original);damaged[-1]^=1;memory.write_bytes(damaged)
   try:m.resume(meta);raise AssertionError('corrupt memory accepted')
   except VMError:pass
   assert not m.alive(meta) and meta['state']=='hibernated'
   memory.write_bytes(original)
   start=time.monotonic();m.resume(meta);elapsed=time.monotonic()-start
   assert ssh('cat /proc/sys/kernel/random/boot_id')==boot
   assert ssh('cat /root/proof')==marker and ssh('cat /dev/shm/proof')==marker
   m.stop(meta,True)
   export=root/'standalone.qcow2';m.disk_crypto.execute(meta,'convert',disk,output=export)
   info=json.loads(subprocess.check_output(['qemu-img','info','--output=json',str(export)]))
   assert info.get('encrypted') and 'backing-filename' not in info
   m.disk_crypto.execute(meta,'check',export)
   m.disk_crypto.execute(meta,'resize',export,size='5G')
   # Simulate the destination using independently loaded key configuration.
   target=DiskCrypto(keyfile);target.execute(meta,'check',export)
   shutil.copyfile(export,disk);meta['disk_gib']=5;m.save(meta);m.start(meta)
   for _ in range(45):
    try:
     if ssh('cat /root/proof')==marker:break
    except Exception:pass
    time.sleep(1)
   else:raise AssertionError('flattened encrypted disk did not boot')
   print(json.dumps({'encrypted':True,'snapshot_memory_restored':True,'same_boot_after_resume':True,'resume_seconds':round(elapsed,3),'encrypted_transfer_and_resize':True,'cold_boot_after_transfer':True,'missing_key_rejected':True}),flush=True)
  except Exception as e:
   print(getattr(e,'diagnostic',str(e)),flush=True)
   if meta:
    print((m.folder(meta)/'hypervisor.log').read_text()[-4000:],flush=True)
   raise
  finally:
   if meta and m.alive(meta):m.stop(meta,True)

if __name__=='__main__':main()
