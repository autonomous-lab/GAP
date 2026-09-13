"""QEMU LUKS payload encryption. No key material in argv, catalog or transfers."""
from contextlib import contextmanager
import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import subprocess

from microvm import VMError

OPTIONS = 'encrypt.format=luks,encrypt.cipher-alg=aes-256,encrypt.cipher-mode=xts,encrypt.key-secret=gapdisk'

class DiskCrypto:
    def __init__(self, path=None):
        self.path=Path(path) if path else None
        if self.path:self.ring()

    def ring(self):
        try:
            if self.path.is_symlink() or self.path.stat().st_mode & 0o077:raise ValueError()
            ring=json.loads(self.path.read_text())
            if ring['active'] not in ring['keys']:raise ValueError()
            for identity,key in ring['keys'].items():
                if not re.fullmatch('[A-Za-z0-9_-]{1,64}',identity) or not re.fullmatch('[0-9a-f]{64}',key):raise ValueError()
            return ring
        except Exception:raise VMError('disk_keyring_unavailable') from None

    def initialize(self,meta):
        if self.path:
            meta['disk_encryption']={'format':'luks','key_id':self.ring()['active'],'salt':os.urandom(32).hex()}

    def key(self,meta):
        config=meta.get('disk_encryption')
        if not config:
            if self.path:raise VMError('unencrypted_disk_rejected')
            return None
        if not self.path:raise VMError('disk_keyring_required')
        try:
            if config['format']!='luks' or not re.fullmatch('[0-9a-f]{64}',config['salt']):raise ValueError()
            master=bytes.fromhex(self.ring()['keys'][config['key_id']])
            binding=json.dumps(['gap-disk-v1',meta['project_id'],meta['owner_did'],meta['vm_id'],config['salt']],separators=(',',':')).encode()
            return hmac.new(master,binding,hashlib.sha256).hexdigest().encode()
        except Exception:raise VMError('disk_key_unavailable') from None

    @contextmanager
    def secret(self,meta):
        key=self.key(meta)
        if key is None:
            yield [],()
            return
        fd=os.memfd_create('gap-disk-key',os.MFD_CLOEXEC)
        try:
            os.write(fd,key);os.lseek(fd,0,0)
            yield ['--object',f'secret,id=gapdisk,file=/proc/self/fd/{fd}'],(fd,)
        finally:os.close(fd)

    def drive(self,meta,path):
        # All paths are worker-owned, subject to MicroVMs' safe-path validation.
        return f'driver=qcow2,file.driver=file,file.filename={path}'+(',encrypt.key-secret=gapdisk' if meta.get('disk_encryption') else '')

    def execute(self,meta,operation,path,output=None,size=None,base=None):
        with self.secret(meta) as (secret,fds):
            encrypted=bool(meta.get('disk_encryption'))
            cmd=['qemu-img',operation,*secret]
            if operation=='create':
                cmd+=['-f','qcow2','-F','raw','-b',str(base)]
                if encrypted:cmd+=['-o',OPTIONS]
                cmd += [str(path),size]
            elif operation=='convert':
                cmd+=['-O','qcow2']
                if encrypted:cmd+=['-o',OPTIONS,'--image-opts',self.drive(meta,path)]
                else:cmd+=['-c',str(path)]
                cmd+=[str(output)]
            elif operation in ('resize','check'):
                cmd+=['--image-opts',self.drive(meta,path)]
                if size:cmd.append(size)
            else:raise VMError('invalid_disk_operation')
            result=subprocess.run(cmd,pass_fds=fds,stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,timeout=900 if operation=='convert' else 120)
            if result.returncode:raise VMError('encrypted_disk_operation_failed:'+operation)
