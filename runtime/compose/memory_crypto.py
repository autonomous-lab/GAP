"""Authenticated streaming hibernation; plaintext never written to a host file."""
import hashlib,hmac,os,socket,struct,threading,time
from pathlib import Path
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from microvm import VMError
MAGIC=b'GAPMEM1\0'
CHUNK=1024*1024

def exact(stream,size):
 data=bytearray()
 while len(data)<size:
  part=stream.read(size-len(data))
  if not part:raise VMError('snapshot_truncated')
  data.extend(part)
 return bytes(data)

def cipher(manager,meta):
 return AESGCM(hmac.new(manager.disk_crypto.key(meta),b'gap-memory-v1',hashlib.sha256).digest())

def seal(source,target,aead,limit):
 prefix=os.urandom(8);target.write(MAGIC+prefix);index=total=0
 while True:
  raw=source.read(CHUNK);total+=len(raw)
  if total>limit:raise VMError('snapshot_too_large')
  nonce=prefix+struct.pack('>I',index);size=struct.pack('>I',len(raw))
  target.write(size+aead.encrypt(nonce,raw,MAGIC+size))
  index+=1
  if not raw:break

def unseal(source,target,aead,limit):
 if exact(source,len(MAGIC))!=MAGIC:raise VMError('snapshot_format_invalid')
 prefix=exact(source,8);index=total=0
 while True:
  size=exact(source,4);length=struct.unpack('>I',size)[0];total+=length
  if length>CHUNK or total>limit:raise VMError('snapshot_too_large')
  try:raw=aead.decrypt(prefix+struct.pack('>I',index),exact(source,length+16),MAGIC+size)
  except Exception:raise VMError('snapshot_authentication_failed') from None
  if not length:
   if source.read(1):raise VMError('snapshot_trailing_data')
   break
  target.write(raw);index+=1

def save(manager,meta):
 folder=manager.folder(meta);path=folder/'memory.enc';pending=folder/'memory.next';sockpath=folder/'memory.sock'
 sockpath.unlink(missing_ok=True)
 aead=cipher(manager,meta);errors=[];stopping=threading.Event();connections=[]
 with socket.socket(socket.AF_UNIX) as listener:
  listener.bind(str(sockpath));listener.listen(1);listener.settimeout(.2)
  def receive():
   try:
    while not stopping.is_set():
     try:conn,_=listener.accept();break
     except socket.timeout:continue
    else:return
    connections.append(conn)
    with conn:
     conn.settimeout(120)
     fd=os.open(pending,os.O_WRONLY|os.O_CREAT|os.O_TRUNC,0o600)
     with conn.makefile('rb') as src,os.fdopen(fd,'wb') as dst:
      seal(src,dst,aead,(meta['memory_mib']+256)*1024**2);dst.flush();os.fsync(dst.fileno())
   except Exception as e:errors.append(e)
  thread=threading.Thread(target=receive,daemon=True);thread.start()
  try:
   manager.qmp(meta,'migrate',{'uri':'unix:'+str(sockpath)})
   deadline=time.monotonic()+120
   while time.monotonic()<deadline:
    status=manager.qmp(meta,'query-migrate').get('status')
    if status=='completed':break
    if status in ('failed','cancelled') or errors:raise VMError('snapshot_stream_failed')
    time.sleep(.05)
   else:raise VMError('snapshot_stream_timeout')
   thread.join(125)
   if thread.is_alive() or errors:raise VMError('snapshot_stream_failed')
   pending.replace(path)
   fd=os.open(folder,os.O_RDONLY|os.O_DIRECTORY)
   try:os.fsync(fd)
   finally:os.close(fd)
  except Exception:
   try:manager.qmp(meta,'migrate_cancel')
   except Exception:pass
   raise
  finally:
   stopping.set()
   for conn in connections:
    try:conn.shutdown(socket.SHUT_RDWR)
    except OSError:pass
   thread.join(2)
   sockpath.unlink(missing_ok=True);pending.unlink(missing_ok=True)

def restore(manager,meta,process):
 path=manager.folder(meta)/'memory.sock';deadline=time.monotonic()+90
 while not path.exists():
  if process.poll() is not None:raise VMError('snapshot_restore_failed')
  if time.monotonic()>deadline:raise VMError('snapshot_restore_timeout')
  time.sleep(.02)
 try:
  with socket.socket(socket.AF_UNIX) as sock:
   sock.settimeout(120);sock.connect(str(path))
   with (manager.folder(meta)/'memory.enc').open('rb') as src,sock.makefile('wb') as dst:
    unseal(src,dst,cipher(manager,meta),(meta['memory_mib']+256)*1024**2);dst.flush()
 finally:path.unlink(missing_ok=True)
