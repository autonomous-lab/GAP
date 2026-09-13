"""Opt-in, disposable TWO-HOST cold disk migration experiment, not a fleet API.

Run in an isolated container, with /trial mounted to a unique test directory.
Only generated test identities may be operated on. No production config is read.
"""
import hashlib,json,os,shutil,subprocess,sys,time,uuid
from pathlib import Path
from microvm import MicroVMs,VMError
from runner import execute_guest
ROOT=Path('/trial')

def manager():return MicroVMs(dict(state_dir='/trial/vm',image_dir='/images',diagnostic_serial=True),execute_guest)
def digest(path):
 h=hashlib.sha256()
 with path.open('rb') as f:
  for b in iter(lambda:f.read(1024*1024),b''):h.update(b)
 return h.hexdigest()
def ssh(m,meta,key,command):
 return subprocess.run(['ssh','-F','/dev/null','-o','BatchMode=yes','-o','IdentitiesOnly=yes','-o','ConnectTimeout=3','-o','StrictHostKeyChecking=yes',
  '-o','UserKnownHostsFile='+str(m.folder(meta)/'known_hosts'),'-o','HostKeyAlias='+meta['vm_id'],'-i',str(key),'-p',str(meta['ssh_port']),
  'root@127.0.0.1',command],text=True,capture_output=True,check=True,timeout=10).stdout.strip()
def ready(m,meta,key):
 for _ in range(75):
  try:ssh(m,meta,key,'true');return
  except subprocess.SubprocessError:time.sleep(1)
 raise AssertionError('guest SSH readiness timeout')
def deny(m,meta):
 try:m.perform(meta['project_id'],meta['owner_did'],'vm/start',dict(request_id=uuid.uuid4().hex,vm_id=meta['vm_id']))
 except VMError as e:assert str(e)=='microvm_suspended_or_policy_unavailable'
 else:raise AssertionError('fenced source restarted')
 assert not m.alive(meta)
def main(stage):
 if os.environ.get('GAP_MIGRATION_SMOKE')!='1':raise RuntimeError('explicit disposable-test opt-in required')
 m=manager();bundle=ROOT/'bundle';meta=None
 if stage=='source':
  assert not (ROOT/'identity.json').exists(),'do not overwrite an existing trial'
  identity=dict(project_id='prj_'+uuid.uuid4().hex[:24],owner_did='did:gap:'+uuid.uuid4().hex*2,transfer_id='move_'+uuid.uuid4().hex,witness=uuid.uuid4().hex)
  (ROOT/'identity.json').write_text(json.dumps(identity));key=ROOT/'test_key'
  subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(key)],check=True)
  try:
   m.perform(identity['project_id'],identity['owner_did'],'vm/create',dict(request_id=uuid.uuid4().hex,vcpus=1,memory_mib=1024,disk_gib=8,ssh_keys=[Path(str(key)+'.pub').read_text().strip()]))
   meta=m.read(identity['project_id'],identity['owner_did']);ready(m,meta,key)
   ssh(m,meta,key,'printf %s '+identity['witness']+' > /root/migration-witness; sync')
   assert ssh(m,meta,key,'cat /root/migration-witness')==identity['witness']
   m.fence_for_migration(identity['project_id'],identity['owner_did'],meta['vm_id'],identity['transfer_id']);deny(m,meta)
   bundle.mkdir(mode=0o700);folder=m.folder(meta)
   subprocess.run(['qemu-img','convert','-c','-O','qcow2',str(folder/'disk.qcow2'),str(bundle/'disk.qcow2')],check=True)
   info=json.loads(subprocess.check_output(['qemu-img','info','--output=json',str(bundle/'disk.qcow2')],text=True));assert 'backing-filename' not in info
   subprocess.run(['qemu-img','check',str(bundle/'disk.qcow2')],check=True,capture_output=True)
   for name in ['client_key','client_key.pub','known_hosts','seed.ext4']:shutil.copy2(folder/name,bundle/name)
   shutil.copytree(folder/'seed',bundle/'seed');shutil.copy2(key,bundle/'test_key')
   receipt=dict(identity=identity,meta=meta,sha256=digest(bundle/'disk.qcow2'))
   (bundle/'receipt.json').write_text(json.dumps(receipt));(ROOT/'receipt.json').write_text(json.dumps(receipt))
   deny(manager(),meta)
   print(json.dumps(dict(stage=stage,vm_id=meta['vm_id'],standalone_disk=True,source_fenced=True,controller_recreation_fenced=True,sha256=receipt['sha256'])),flush=True)
  finally:
   if meta and m.alive(meta):m.stop(meta,True)
 elif stage=='target':
  receipt=json.loads((bundle/'receipt.json').read_text());identity=receipt['identity'];meta=dict(receipt['meta'])
  assert digest(bundle/'disk.qcow2')==receipt['sha256'],'transferred disk digest mismatch'
  assert not m.read(meta['project_id'],meta['owner_did']),'target identity already exists'
  meta.update(state='stopped',catalog_key=meta['project_id'],ssh_port=m.reserved_port(),ports=[],image_version=m.image_version())
  for k in ['pid','snapshot_tag','snapshot_qemu_version']:meta.pop(k,None)
  folder=m.folder(meta);folder.mkdir(mode=0o700,parents=True)
  for name in ['disk.qcow2','client_key','client_key.pub','known_hosts','seed.ext4']:shutil.copy2(bundle/name,folder/name)
  shutil.copytree(bundle/'seed',folder/'seed');m.save(meta)
  try:
   m.perform(meta['project_id'],meta['owner_did'],'vm/start',dict(request_id=uuid.uuid4().hex,vm_id=meta['vm_id']))
   ready(m,meta,bundle/'test_key');assert ssh(m,meta,bundle/'test_key','cat /root/migration-witness')==identity['witness']
   print(json.dumps(dict(stage=stage,vm_id=meta['vm_id'],digest_verified=True,witness_verified=True,booted_on_target=True)),flush=True)
  finally:
   if m.alive(meta):m.stop(meta,True)
 elif stage=='verify-target':
  receipt=json.loads((bundle/'receipt.json').read_text());identity=receipt['identity']
  meta=m.read(receipt['meta']['project_id'],receipt['meta']['owner_did'],receipt['meta']['vm_id'])
  assert meta and digest(bundle/'disk.qcow2')==receipt['sha256']
  try:
   m.perform(meta['project_id'],meta['owner_did'],'vm/start',dict(request_id=uuid.uuid4().hex,vm_id=meta['vm_id']))
   ready(m,meta,bundle/'test_key');assert ssh(m,meta,bundle/'test_key','cat /root/migration-witness')==identity['witness']
   result=dict(stage=stage,vm_id=meta['vm_id'],digest_verified=True,witness_verified=True,booted_on_target=True)
   (ROOT/'result.json').write_text(json.dumps(result));print(json.dumps(result),flush=True)
  finally:
   if m.alive(meta):m.stop(meta,True)
 elif stage=='verify-source':
  receipt=json.loads((ROOT/'receipt.json').read_text());meta=m.read(receipt['meta']['project_id'],receipt['meta']['owner_did'],receipt['meta']['vm_id']);deny(m,meta)
  print(json.dumps(dict(stage=stage,source_restart_refused=True)),flush=True)
 else:raise ValueError('unknown stage')
if __name__=='__main__':main(sys.argv[1])
