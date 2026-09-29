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
import shlex
import threading
import time
import urllib.error
import urllib.request

import asyncssh


class GatewayError(Exception):
    pass


def admission_error(message):
    if message=='anonymous trial capacity reached':
        return 'capacity_reached'
    if message in ('one anonymous VM per IP','anonymous trial limit reached') or 'free_vm_ip_already_reserved' in message:
        return 'trial_already_reserved'
    if 'free_vm_key_already_reserved' in message:
        return 'key_reserved_on_another_node'
    if 'free_vm_expired_pending_cleanup' in message:
        return 'cleanup_pending'
    if message=='anonymous trial already claimed' or 'free_vm_already_claimed' in message:
        return 'trial_already_claimed'
    return 'admission_or_node_unavailable'


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
    except urllib.error.HTTPError as error:
        try:
            response=json.loads(error.read(8192))
            message=response.get('error',{}).get('message','')
            if not isinstance(message,str):raise ValueError()
        except (AttributeError,ValueError,TypeError,UnicodeDecodeError):
            message=''
        raise GatewayError(admission_error(message)) from None
    except Exception:
        raise GatewayError('admission_or_node_unavailable') from None


def rpc_wait(runner,project,owner,action,method,body,timeout=120):
    status,result=runner.rpc({'project_id':project,'owner_did':owner,'action':action,
                              'method':method,'body':body})
    if status not in (200,202):raise GatewayError('vm_operation_rejected')
    if result.get('status') in ('succeeded','failed'):
        if result['status']=='failed':
            if result.get('result',{}).get('error')=='microvm_credits_or_budget_exhausted':
                raise GatewayError('claimed_vm_no_credits')
            raise GatewayError('vm_operation_failed')
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
        if value['status'] in ('failed','interrupted'):
            if value.get('result',{}).get('error')=='microvm_credits_or_budget_exhausted':
                raise GatewayError('claimed_vm_no_credits')
            raise GatewayError('vm_operation_failed')
    raise GatewayError('vm_operation_timeout')


def stable_id(project,operation):
    return hashlib.sha256((project+'\0'+operation).encode()).hexdigest()[:32]


def interactive_shell_command(active_until):
    """Keep the server-issued deadline visible without adding prompt noise."""
    deadline=int(active_until)
    prompt=(f'gap_left=$(({deadline} - $(date +%s))); '
            'if (( gap_left < 0 )); then gap_left=0; fi; '
            'printf -v gap_time "%dm %02ds" "$((gap_left / 60))" "$((gap_left % 60))"; '
            'PS1="GAP · ${gap_time} left · \\w ❯ "')
    return 'env PROMPT_COMMAND=' + shlex.quote(prompt) + ' bash --login -i'


def welcome_banner(details,remaining,colored=False):
    reset='\x1b[0m' if colored else ''
    brand='\x1b[1;36m' if colored else ''
    accent='\x1b[1;32m' if colored else ''
    muted='\x1b[2m' if colored else ''
    left=f'{remaining//60}m {remaining%60:02d}s'
    return (f'\r\n  {brand}GAP  /  FREE MICROVM{reset}\r\n'
            f'  Your workspace is ready  ·  1 vCPU  ·  1 GiB  ·  {accent}{left} left{reset}\r\n'
            f'\r\n'
            f'  {muted}PREVIEW{reset}      {details["preview"]["preview_url"]}\r\n'
            f'  {muted}BASIC AUTH{reset}   {details["preview"]["username"]} / {details["preview"]["password"]}\r\n'
            f'  {muted}CLAIM{reset}        {details["claim_url"]}\r\n'
            f'\r\n'
            f'  Serve your app on port 8080 to use the preview.\r\n'
            f'  After the VM stops, you have 24 hours to claim it.\r\n\r\n')


def claimable_banner(details,now,colored=False):
    reset='\x1b[0m' if colored else ''
    brand='\x1b[1;36m' if colored else ''
    accent='\x1b[1;32m' if colored else ''
    remaining=max(0,details['claim_until']-now)
    minutes=(remaining+59)//60
    return (f'\r\n  {brand}GAP  /  TRIAL COMPLETE{reset}\r\n'
            f'  Your MicroVM has stopped after its one-hour free session.\r\n'
            f'  You have {accent}{minutes//60}h {minutes%60:02d}m left to claim it{reset} and keep its files.\r\n'
            f'\r\n'
            f'  CLAIM        {details["claim_url"]}\r\n'
            f'\r\n'
            f'  Create or sign in to your GAP account and verify your email.\r\n'
            f'  After the claim window, unclaimed data is deleted; you can then\r\n'
            f'  start a new free VM once cleanup is complete.\r\n\r\n')


def claimed_banner(details,colored=False):
    brand='\x1b[1;36m' if colored else ''
    reset='\x1b[0m' if colored else ''
    memory=details['memory_mib']
    memory_label=f'{memory//1024} GiB' if memory%1024==0 else f'{memory} MiB'
    app_ports=', '.join(str(port) for port in details['app_ports']) or 'none configured'
    return (f'\r\n  {brand}GAP  /  YOUR MICROVM{reset}\r\n'
            f'  Reconnected. Your files are where you left them.\r\n'
            f'\r\n'
            f'  COMPUTE     {details["vcpus"]:g} vCPU  ·  {memory_label} RAM  ·  {details["disk_gib"]} GiB disk\r\n'
            f'  APP PORTS   {app_ports} (inside VM)\r\n'
            f'  SSH ENTRY   port 2121\r\n'
            f'  MANAGE      {details["manage_url"]}\r\n\r\n')


def gateway_error_message(error):
    return {
        'capacity_reached':'All free VM slots on this node are in use. Try another node or try again later.',
        'trial_already_reserved':'This IP or SSH key already has an unclaimed free VM. Reconnect with its original key on its original node to see the claim link.',
        'key_reserved_on_another_node':'This SSH key has a free VM on another node. Reconnect to that node to see its claim link.',
        'cleanup_pending':'Your previous trial has ended. Secure cleanup is still in progress; try again shortly.',
        'trial_already_claimed':'This SSH key belongs to a claimed VM. Sign in to GAP to manage it.',
        'claimed_vm_unavailable':'Your claimed VM is unavailable. Sign in to GAP to check its status and billing.',
        'claimed_vm_no_credits':'Your claimed VM cannot start because its project has no available credits. Check your GAP account billing.',
    }.get(str(error),'Unable to start a free VM right now. Please try again later.')


def retry_mutation(runner,project,owner,action,method,body,timeout=90):
    deadline=time.monotonic()+timeout
    while True:
        try:
            return rpc_wait(runner,project,owner,action,method,
                dict(body,request_id=secrets.token_hex(16)),timeout=30)
        except GatewayError as error:
            if str(error)=='claimed_vm_no_credits':raise
            if time.monotonic()+2>=deadline:raise
            time.sleep(2)


def prepare(runner,key,ip):
    admission=request_node(runner,'/internal/free-vm/reserve',{'ssh_key':key,'source_ip':ip})
    if admission.get('status')=='claimable':
        if not isinstance(admission.get('claim_url'),str) or not isinstance(admission.get('claim_until'),int):
            raise GatewayError('invalid_admission')
        return admission
    project,owner=admission['project_id'],admission['owner_did']
    if not re.fullmatch(r'prj_[0-9a-f]{24}',project) or not re.fullmatch(r'did:gap:[0-9a-f]{64}',owner):
        raise GatewayError('invalid_admission')
    if admission.get('status')=='claimed':
        manager=runner.hypervisor
        meta=manager.read(project,owner)
        if not meta or meta.get('guest_image')!='free-vm-v2' or meta.get('state')=='destroyed':
            raise GatewayError('claimed_vm_unavailable')
        if meta.get('state') in ('stopped','hibernated') or (meta.get('state')=='running' and not manager.alive(meta)):
            rpc_wait(runner,project,owner,'vm/resume' if meta['state']=='hibernated' else 'vm/start',
                'POST',{'request_id':secrets.token_hex(16),'vm_id':meta['vm_id']},timeout=180)
            meta=manager.read(project,owner)
        if not meta or meta.get('state')!='running' or meta.get('tier') not in ('trial','approved'):
            raise GatewayError('claimed_vm_unavailable')
        retry_mutation(runner,project,owner,'terminal/prepare','POST',{'vm_id':meta['vm_id']})
        folder=manager.folder(meta)
        key_file=folder/'terminal_key'
        host_key=(folder/'seed'/'ssh_host_ed25519_key.pub').read_text().strip()
        if not key_file.is_file() or not host_key.startswith('ssh-ed25519 '):
            raise GatewayError('guest_ssh_identity_unavailable')
        return dict(admission,vm_id=meta['vm_id'],ssh_port=meta['ssh_port'],
            guest_key=str(key_file),guest_host_key=host_key,
            vcpus=meta['vcpus'],memory_mib=meta['memory_mib'],disk_gib=meta['disk_gib'],
            app_ports=sorted({port['guest_port'] for port in meta['ports']}),
            manage_url='https://gap.geta.team/account')
    if admission.get('status') not in (None,'active'):
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
        # Keep successful SSH logins clean. Key-generation help is on /free-vm.
        return True

    def public_key_auth_supported(self):
        return True

    def validate_public_key(self,username,key):
        if username!='free' or key.get_algorithm() not in ('ssh-ed25519','ssh-rsa'):return False
        if key.get_algorithm()=='ssh-rsa' and not 2048<=key.pyca_key.key_size<=8192:return False
        exported=key.export_public_key().decode().strip()
        if not re.fullmatch(r'(ssh-ed25519|ssh-rsa) [A-Za-z0-9+/=]{60,2048}',exported):return False
        self.conn.set_extra_info(free_vm_public_key=exported)
        return True

    def connection_requested(self,*args):
        return False

    def server_requested(self,*args):
        return False


async def bridge(source,destination,close_on_eof=False):
    while True:
        chunk=await source.read(65536)
        if not chunk:break
        destination.write(chunk)
        await destination.drain()
    if close_on_eof:
        destination.write_eof()


async def shell(runner,process):
    conn=process.get_extra_info('connection')
    key=conn.get_extra_info('free_vm_public_key') if conn else None
    peer=process.get_extra_info('peername')
    if not key or not peer or not isinstance(peer[0],str):
        process.stderr.write(b'Invalid authentication.\r\n');process.exit(1);return
    if process.subsystem is not None:
        process.stderr.write(b'SSH subsystems are disabled.\r\n');process.exit(1);return
    try:
        details=await asyncio.to_thread(prepare,runner,key,peer[0])
    except Exception as error:
        print('free_vm_provision_error:',type(error).__name__,str(error)[:160],flush=True)
        process.stderr.write((gateway_error_message(error)+'\r\n').encode())
        process.exit(1);return
    if details.get('status')=='claimable':
        process.stdout.write(claimable_banner(details,int(time.time()),process.term_type is not None).encode())
        await process.stdout.drain()
        process.exit(1 if process.command is not None else 0)
        return
    claimed=details.get('status')=='claimed'
    if not claimed and time.time()>=details['active_until']:
        process.stderr.write(b'The active trial has ended.\r\n');process.exit(1);return
    known=asyncssh.import_known_hosts(f"[127.0.0.1]:{details['ssh_port']} {details['guest_host_key']}\n")
    try:
        async with asyncssh.connect('127.0.0.1',port=details['ssh_port'],username='root',
                client_keys=[details['guest_key']],known_hosts=known,encoding=None,
                agent_path=None,connect_timeout=10) as guest:
            command=process.command if process.command is not None else (
                'bash --login -i' if claimed else interactive_shell_command(details['active_until']))
            remote=await guest.create_process(command,term_type=process.term_type,
                term_size=process.term_size,encoding=None)
            if claimed:
                panel=claimed_banner(details,process.term_type is not None)
            else:
                remaining=max(0,int(details['active_until']-time.time()))
                panel=welcome_banner(details,remaining,process.term_type is not None)
            process.stdout.write(panel.encode())
            await process.stdout.drain()
            input_task=asyncio.create_task(bridge(process.stdin,remote.stdin,True))
            output_tasks=[asyncio.create_task(bridge(remote.stdout,process.stdout)),
                          asyncio.create_task(bridge(remote.stderr,process.stderr))]
            try:
                if claimed:
                    await remote.wait()
                else:
                    await asyncio.wait_for(remote.wait(),timeout=max(1,details['active_until']-time.time()))
                await asyncio.wait_for(asyncio.gather(*output_tasks),timeout=5)
            finally:
                input_task.cancel()
                for task in output_tasks:task.cancel()
                await asyncio.gather(input_task,*output_tasks,return_exceptions=True)
            process.exit(remote.exit_status if remote.exit_status is not None else 0)
    except (asyncssh.Error,OSError,asyncio.TimeoutError):
        process.stderr.write(b'VM connection interrupted.\r\n')
        process.exit(1)


async def serve(runner,port,host_key):
    server=await asyncssh.listen('0.0.0.0',port,server_factory=FreeServer,
        server_host_keys=[str(host_key)],process_factory=lambda process:shell(runner,process),
        encoding=None,agent_forwarding=False,x11_forwarding=False,
        signature_algs=['ssh-ed25519','rsa-sha2-512','rsa-sha2-256'])
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
