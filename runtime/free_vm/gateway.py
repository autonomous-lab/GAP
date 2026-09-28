"""Key-only SSH entry for one-hour anonymous MicroVMs.

Provisioning happens only after AsyncSSH has verified possession of the private
key. Merely offering a public key during authentication allocates nothing.
"""
import asyncio
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import threading
import time
import urllib.request

import asyncssh


class GatewayError(Exception):
    pass


def request_node(runner, endpoint, body):
    from runner import load_config, NoRedirect
    config=load_config(runner.path)
    request=urllib.request.Request(config['node_url'].rstrip('/')+endpoint,
        data=json.dumps(body).encode(),
        headers={'Authorization':'Bearer '+runner.token,'Content-Type':'application/json'})
    try:
        with urllib.request.build_opener(NoRedirect).open(request,timeout=5) as response:
            result=json.loads(response.read(8192))
            if response.status!=200 or not isinstance(result,dict):raise ValueError()
            return result
    except Exception:
        raise GatewayError('admission_or_node_unavailable') from None


def rpc_wait(runner,project,owner,action,method,body,timeout=120):
    status,result=runner.rpc({'project_id':project,'owner_did':owner,'action':action,
                              'method':method,'body':body})
    if status not in (200,202):raise GatewayError('vm_operation_rejected')
    if result.get('status') in ('succeeded','failed'):
        if result['status']=='failed':raise GatewayError('vm_operation_failed')
        return result.get('result',{})
    job=result.get('job_id')
    if not isinstance(job,str):raise GatewayError('vm_job_missing')
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        time.sleep(.25)
        status,value=runner.rpc({'project_id':project,'owner_did':owner,
            'action':'jobs/'+job,'method':'GET','body':{}})
        if status!=200:raise GatewayError('vm_job_unavailable')
        if value['status']=='succeeded':return value['result']
        if value['status'] in ('failed','interrupted'):raise GatewayError('vm_operation_failed')
    raise GatewayError('vm_operation_timeout')


def stable_id(project,operation):
    return hashlib.sha256((project+'\0'+operation).encode()).hexdigest()[:32]


def retry_mutation(runner,project,owner,action,method,body,timeout=90):
    deadline=time.monotonic()+timeout
    while True:
        try:
            return rpc_wait(runner,project,owner,action,method,
                dict(body,request_id=secrets.token_hex(16)),timeout=30)
        except GatewayError:
            if time.monotonic()+2>=deadline:raise
            time.sleep(2)


def prepare(runner,key,ip):
    admission=request_node(runner,'/internal/free-vm/reserve',{'ssh_key':key,'source_ip':ip})
    project,owner=admission['project_id'],admission['owner_did']
    if not re.fullmatch(r'prj_[0-9a-f]{24}',project) or not re.fullmatch(r'did:gap:[0-9a-f]{64}',owner):
        raise GatewayError('invalid_admission')
    if time.time()>=admission['active_until']:
        raise GatewayError('trial_hour_finished')
    manager=runner.hypervisor
    meta=manager.read(project,owner)
    if meta is None:
        rpc_wait(runner,project,owner,'vm','POST',{'request_id':stable_id(project,'create'),
            'vcpus':1,'memory_mib':1024,'disk_gib':8,'ports':[8080],
            'ssh_keys':[key],'start':True,'execution_mode':'serverless'},timeout=180)
        meta=manager.read(project,owner)
    if meta and meta.get('tier')=='anonymous' and (meta.get('state')=='stopped' or
            (meta.get('state')=='running' and not manager.alive(meta))) and time.time()<admission['active_until']:
        rpc_wait(runner,project,owner,'vm/start','POST',{'request_id':secrets.token_hex(16),
            'vm_id':meta['vm_id']},timeout=180)
        meta=manager.read(project,owner)
    if not meta or meta['state']!='running' or meta.get('tier')!='anonymous' or meta.get('anonymous_until')!=admission['active_until']:
        raise GatewayError('trial_vm_unavailable')
    if runner.ingress:
        retry_mutation(runner,project,owner,'ingress','PUT',
            {'vm_id':meta['vm_id'],'enabled':True,'guest_port':8080})
    retry_mutation(runner,project,owner,'terminal/prepare','POST',{'vm_id':meta['vm_id']})
    preview=request_node(runner,'/internal/free-vm/preview',{'project_id':project,
        'owner_did':owner,'vm_id':meta['vm_id']})
    folder=manager.folder(meta)
    key_file=folder/'terminal_key'
    host_key=(folder/'seed'/'ssh_host_ed25519_key.pub').read_text().strip()
    if not key_file.is_file() or not host_key.startswith('ssh-ed25519 '):
        raise GatewayError('guest_ssh_identity_unavailable')
    return dict(admission,vm_id=meta['vm_id'],ssh_port=meta['ssh_port'],
                guest_key=str(key_file),guest_host_key=host_key,preview=preview)


class FreeServer(asyncssh.SSHServer):
    def connection_made(self,conn):
        self.conn=conn

    def begin_auth(self,username):
        return True

    def public_key_auth_supported(self):
        return True

    def validate_public_key(self,username,key):
        if username!='free' or key.get_algorithm()!='ssh-ed25519':return False
        exported=key.export_public_key().decode().strip()
        if not re.fullmatch(r'ssh-ed25519 [A-Za-z0-9+/=]{60,128}',exported):return False
        self.conn.set_extra_info(free_vm_public_key=exported)
        return True

    def connection_requested(self,*args):
        return False

    def server_requested(self,*args):
        return False


async def bridge(source,destination):
    while True:
        chunk=await source.read(65536)
        if not chunk:break
        destination.write(chunk)
        await destination.drain()


async def shell(runner,process):
    conn=process.get_extra_info('connection')
    key=conn.get_extra_info('free_vm_public_key') if conn else None
    peer=process.get_extra_info('peername')
    if not key or not peer or not isinstance(peer[0],str):
        process.stderr.write(b'Authentification invalide.\r\n');process.exit(1);return
    if process.subsystem is not None:
        process.stderr.write(b'Les sous-syst\xc3\xa8mes SSH sont d\xc3\xa9sactiv\xc3\xa9s.\r\n');process.exit(1);return
    try:
        details=await asyncio.to_thread(prepare,runner,key,peer[0])
    except Exception as error:
        print('free_vm_provision_error:',type(error).__name__,str(error)[:160],flush=True)
        process.stderr.write(b'VM indisponible ou limite atteinte. R\xc3\xa9essayez plus tard.\r\n')
        process.exit(1);return
    if time.time()>=details['active_until']:
        process.stderr.write(b'La p\xc3\xa9riode active est termin\xc3\xa9e.\r\n');process.exit(1);return
    remaining=max(0,int(details['active_until']-time.time()))
    banner=(f"\r\nGAP MicroVM · 1 vCPU / 1 Gio · {remaining//60} min restantes\r\n"
            f"Preview : {details['preview']['preview_url']}\r\n"
            f"Basic Auth : {details['preview']['username']} / {details['preview']['password']}\r\n"
            f"Réclamer : {details['claim_url']}\r\n\r\n")
    process.stdout.write(banner.encode());await process.stdout.drain()
    known=asyncssh.import_known_hosts(f"[127.0.0.1]:{details['ssh_port']} {details['guest_host_key']}\n")
    try:
        async with asyncssh.connect('127.0.0.1',port=details['ssh_port'],username='root',
                client_keys=[details['guest_key']],known_hosts=known,encoding=None,
                agent_path=None,connect_timeout=10) as guest:
            command=process.command
            remote=await guest.create_process(command,term_type=process.term_type,
                term_size=process.term_size,encoding=None)
            input_task=asyncio.create_task(bridge(process.stdin,remote.stdin))
            output_tasks=[asyncio.create_task(bridge(remote.stdout,process.stdout)),
                          asyncio.create_task(bridge(remote.stderr,process.stderr))]
            try:
                await asyncio.wait_for(remote.wait(),timeout=max(1,details['active_until']-time.time()))
                await asyncio.wait_for(asyncio.gather(*output_tasks),timeout=5)
            finally:
                input_task.cancel()
                for task in output_tasks:task.cancel()
                await asyncio.gather(input_task,*output_tasks,return_exceptions=True)
            process.exit(remote.exit_status if remote.exit_status is not None else 0)
    except (asyncssh.Error,OSError,asyncio.TimeoutError):
        process.stderr.write(b'Connexion de la VM interrompue.\r\n')
        process.exit(1)


async def serve(runner,port,host_key):
    server=await asyncssh.listen('0.0.0.0',port,server_factory=FreeServer,
        server_host_keys=[str(host_key)],process_factory=lambda process:shell(runner,process),
        encoding=None,agent_forwarding=False,x11_forwarding=False)
    await server.wait_closed()


def start(runner,config):
    settings=config.get('free_vm',{})
    if settings.get('enabled') is not True:return
    port=settings.get('ssh_port',2121)
    if type(port) is not int or port!=2121 or runner.hypervisor is None or runner.runtime is None:
        raise ValueError('invalid_free_vm_gateway_configuration')
    key=Path(config['state_dir'])/'free-vm-ssh-host-key'
    if not key.exists():
        pending=key.with_suffix('.new')
        pending.write_bytes(asyncssh.generate_private_key('ssh-ed25519').export_private_key())
        os.chmod(pending,0o600);pending.replace(key)
    elif key.stat().st_mode & 0o077:
        raise ValueError('insecure_free_vm_host_key')
    thread=threading.Thread(target=lambda:asyncio.run(serve(runner,port,key)),daemon=True)
    thread.start()
    return thread
