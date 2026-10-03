"""Key-only SSH entry for one-hour anonymous MicroVMs.

Provisioning happens only after AsyncSSH has verified possession of the private
key. Merely offering a public key during authentication allocates nothing.
"""
import asyncio
import base64
import binascii
import hashlib
import ipaddress
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
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey


class GatewayError(Exception):
    pass


_used_relay_nonces = {}


def key_hash(public_key):
    parts=public_key.split()
    if len(parts)!=2 or parts[0] not in ('ssh-ed25519','ssh-rsa'):
        raise GatewayError('invalid_relay_key')
    try:
        wire=base64.b64decode(parts[1],validate=True)
    except (ValueError,TypeError,binascii.Error):
        raise GatewayError('invalid_relay_key') from None
    return hashlib.sha256(wire).hexdigest()


def verify_relay_ticket(ticket,ssh_key,relay_key,source_ip,settings):
    """Accept only a short, one-use authority grant for this exact SSH hop."""
    try:
        prefix,encoded,signature=ticket.split('.')
        if prefix!='gapr1' or len(ticket)>4096:
            raise ValueError()
        signed=(prefix+'.'+encoded).encode()
        payload=base64.urlsafe_b64decode(encoded+'='*(-len(encoded)%4))
        signature=base64.urlsafe_b64decode(signature+'='*(-len(signature)%4))
        Ed25519PublicKey.from_public_bytes(bytes.fromhex(settings['fleet_public_key'])).verify(signature,signed)
        claims=json.loads(payload)
        if set(claims)!={'v','home','ingress','ssh','relay','ip','project','iat','exp','nonce'}:
            raise ValueError()
        now=int(time.time())
        address=ipaddress.ip_address(source_ip)
        if address.version==6 and address.ipv4_mapped:
            address=address.ipv4_mapped
        if (claims['v']!=1 or claims['home']!=settings['node_id']
                or claims['ingress'] not in settings['peers']
                or claims['ssh']!=key_hash(ssh_key)
                or claims['relay']!=key_hash(relay_key)
                or claims['ip']!=str(address) or not address.is_global
                or not re.fullmatch(r'prj_[0-9a-f]{24}',claims['project'])
                or type(claims['iat']) is not int or type(claims['exp']) is not int
                or claims['iat']>now+5 or not now<claims['exp']<=claims['iat']+60
                or not isinstance(claims['nonce'],str)
                or not re.fullmatch(r'[0-9a-f]{32}',claims['nonce'])):
            raise ValueError()
        for nonce,expiry in list(_used_relay_nonces.items()):
            if expiry<=now:
                del _used_relay_nonces[nonce]
        if claims['nonce'] in _used_relay_nonces or len(_used_relay_nonces)>=10000:
            raise ValueError()
        _used_relay_nonces[claims['nonce']]=claims['exp']
        return claims
    except (ValueError,TypeError,KeyError,IndexError,UnicodeError):
        raise GatewayError('relay_ticket_invalid') from None
    except Exception:
        raise GatewayError('relay_ticket_invalid') from None


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
                raise GatewayError(credit_block_reason(runner,project,owner))
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
                raise GatewayError(credit_block_reason(runner,project,owner))
            raise GatewayError('vm_operation_failed')
    raise GatewayError('vm_operation_timeout')


def credit_block_reason(runner,project,owner):
    """Turn a generic worker denial into the user's actionable billing cause."""
    try:
        status,view=runner.rpc({'project_id':project,'owner_did':owner,
                                'action':'credits','method':'GET','body':{}})
        if status==200 and isinstance(view,dict):
            alerts=view.get('alerts',[])
            if isinstance(alerts,list) and 'budget_exhausted' in alerts:
                return 'claimed_vm_budget_exhausted'
            if (isinstance(alerts,list) and 'credits_exhausted' in alerts
                    or view.get('fleet',{}).get('funding_status')=='exhausted'):
                return 'claimed_vm_no_credits'
    except Exception:
        pass
    return 'claimed_vm_funding_blocked'


def stable_id(project,operation):
    return hashlib.sha256((project+'\0'+operation).encode()).hexdigest()[:32]


def interactive_shell_command(active_until,persistent=False):
    """Keep the server-issued deadline visible without adding prompt noise."""
    deadline=int(active_until)
    prompt=(f'gap_left=$(({deadline} - $(date +%s))); '
            'if (( gap_left < 0 )); then gap_left=0; fi; '
            'printf -v gap_time "%dm %02ds" "$((gap_left / 60))" "$((gap_left % 60))"; '
            'PS1="GAP · ${gap_time} left · \\w ❯ "')
    shell=('tmux new-session -A -s gap -c /app ' + shlex.quote('bash --login -i')) if persistent else 'bash --login -i'
    return 'env PROMPT_COMMAND=' + shlex.quote(prompt) + ' ' + shell


def welcome_banner(details,remaining,colored=False,ready_seconds=None):
    reset='\x1b[0m' if colored else ''
    brand='\x1b[1;36m' if colored else ''
    accent='\x1b[1;32m' if colored else ''
    muted='\x1b[2m' if colored else ''
    left=f'{remaining//60}m {remaining%60:02d}s'
    ready=(f'  {accent}✓ Ready in {ready_seconds}s{reset}\r\n\r\n' if ready_seconds is not None else '')
    detach=('  Press Ctrl+B, then D to detach; reconnect with the same SSH key.\r\n'
            if details.get('guest_image')=='free-vm-v3' else
            '  Type exit to disconnect; reconnect with the same SSH key.\r\n')
    feature=('  │  Run `opencode` to build with free AI models.         │\r\n'
             if details.get('guest_image')=='free-vm-v3' else
             '  │  Docker, Python, Node, Go and PHP are ready to use.  │\r\n')
    return (f'\r\n{ready}  {brand}Welcome to your GAP MicroVM.{reset} You have {accent}{left} left{reset} to build.\r\n'
            f'  1 vCPU · 1 GiB RAM · 8 GiB disk. Run your app on $PORT (8080).\r\n'
            f'\r\n'
            f'  {muted}• Claim:{reset}       {details["claim_url"]}\r\n'
            f'  {muted}• Preview:{reset}     {details["preview"]["preview_url"]}\r\n'
            f'  {muted}• Basic Auth:{reset}  {details["preview"]["username"]} / {details["preview"]["password"]}\r\n'
            f'\r\n'
            f'  ┌────────────────────────────────────────────────────────┐\r\n'
            f'{feature}'
            f'  └────────────────────────────────────────────────────────┘\r\n'
            f'\r\n{detach}'
            f'  After the hour, you have 24 hours to claim your files.\r\n\r\n')


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
        'claimed_vm_no_credits':'Your VM is paused: account credits are exhausted. Open billing: https://gap.geta.team/account#billing',
        'claimed_vm_budget_exhausted':'Your VM is paused: the project spending budget is reached. Review your VM budget: https://gap.geta.team/account#machines',
        'claimed_vm_funding_blocked':'Your VM is paused by a funding limit. Review billing and your project budget: https://gap.geta.team/account#billing',
    }.get(str(error),'Unable to start a free VM right now. Please try again later.')


def retry_mutation(runner,project,owner,action,method,body,timeout=90):
    deadline=time.monotonic()+timeout
    while True:
        try:
            return rpc_wait(runner,project,owner,action,method,
                dict(body,request_id=secrets.token_hex(16)),timeout=30)
        except GatewayError as error:
            if str(error) in ('claimed_vm_no_credits','claimed_vm_budget_exhausted',
                              'claimed_vm_funding_blocked'):raise
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
        if not meta or meta.get('guest_image') not in ('free-vm-v2','free-vm-v3') or meta.get('state')=='destroyed':
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
        return dict(admission,vm_id=meta['vm_id'],ssh_port=meta['ssh_port'],guest_image=meta['guest_image'],
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
    return dict(admission,vm_id=meta['vm_id'],ssh_port=meta['ssh_port'],guest_image=meta['guest_image'],
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
        if username not in ('free','relay') or key.get_algorithm() not in ('ssh-ed25519','ssh-rsa'):return False
        if username=='relay' and key.get_algorithm()!='ssh-ed25519':return False
        if key.get_algorithm()=='ssh-rsa' and not 2048<=key.pyca_key.key_size<=8192:return False
        exported=key.export_public_key().decode().strip()
        if not re.fullmatch(r'(ssh-ed25519|ssh-rsa) [A-Za-z0-9+/=]{60,2048}',exported):return False
        if username=='relay':
            self.conn.set_extra_info(free_vm_relay_key=exported)
        else:
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


async def local_shell(runner,process,key,ip,user_command,expected_project=None):
    started=time.monotonic()
    try:
        details=await asyncio.to_thread(prepare,runner,key,ip)
    except Exception as error:
        print('free_vm_provision_error:',type(error).__name__,str(error)[:160],flush=True)
        process.stderr.write((gateway_error_message(error)+'\r\n').encode())
        process.exit(1);return
    if expected_project and details.get('project_id',expected_project)!=expected_project:
        process.stderr.write(b'Fleet route identity mismatch.\r\n');process.exit(1);return
    if details.get('status')=='claimable':
        process.stdout.write(claimable_banner(details,int(time.time()),process.term_type is not None).encode())
        await process.stdout.drain()
        process.exit(1 if user_command is not None else 0)
        return
    claimed=details.get('status')=='claimed'
    if not claimed and time.time()>=details['active_until']:
        process.stderr.write(b'The active trial has ended.\r\n');process.exit(1);return
    known=asyncssh.import_known_hosts(f"[127.0.0.1]:{details['ssh_port']} {details['guest_host_key']}\n")
    try:
        async with asyncssh.connect('127.0.0.1',port=details['ssh_port'],username='root',
                client_keys=[details['guest_key']],known_hosts=known,encoding=None,
                agent_path=None,connect_timeout=10) as guest:
            persistent=details.get('guest_image')=='free-vm-v3' and process.term_type is not None
            command=user_command if user_command is not None else (
                ('tmux new-session -A -s gap -c /app '+shlex.quote('bash --login -i') if persistent else 'bash --login -i')
                if claimed else interactive_shell_command(details['active_until'],persistent))
            remote=await guest.create_process(command,term_type=process.term_type,
                term_size=process.term_size,encoding=None)
            if claimed:
                panel=claimed_banner(details,process.term_type is not None)
            else:
                remaining=max(0,int(details['active_until']-time.time()))
                panel=welcome_banner(details,remaining,process.term_type is not None,
                                     max(1,round(time.monotonic()-started)))
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


async def relay_shell(process,key,ip,user_command,routing,settings):
    target=routing.get('node_id')
    peer=settings['peers'].get(target)
    if not peer or not isinstance(routing.get('ticket'),str):
        raise GatewayError('fleet_route_unavailable')
    payload={'ticket':routing['ticket'],'ssh_key':key,'source_ip':ip,
             'command':user_command}
    encoded=base64.urlsafe_b64encode(json.dumps(payload,separators=(',',':')).encode()).decode().rstrip('=')
    if len(encoded)>8192:
        raise GatewayError('fleet_route_unavailable')
    host=peer['host']
    port=peer.get('port',2121)
    known=asyncssh.import_known_hosts(f'[{host}]:{port} {peer["host_key"]}\n')
    try:
        async with asyncssh.connect(host,port=port,username='relay',
                client_keys=[str(settings['relay_key_file'])],known_hosts=known,
                agent_path=None,encoding=None,connect_timeout=10) as relay:
            remote=await relay.create_process('gap-relay-v1 '+encoded,
                term_type=process.term_type,term_size=process.term_size,encoding=None)
            tasks=[asyncio.create_task(bridge(process.stdin,remote.stdin,True)),
                   asyncio.create_task(bridge(remote.stdout,process.stdout)),
                   asyncio.create_task(bridge(remote.stderr,process.stderr))]
            try:
                await remote.wait()
                await asyncio.wait_for(asyncio.gather(*tasks[1:]),timeout=5)
            finally:
                for task in tasks:task.cancel()
                await asyncio.gather(*tasks,return_exceptions=True)
            process.exit(remote.exit_status if remote.exit_status is not None else 0)
    except (asyncssh.Error,OSError,asyncio.TimeoutError):
        raise GatewayError('fleet_route_unavailable') from None


async def shell(runner,process,settings=None):
    conn=process.get_extra_info('connection')
    key=conn.get_extra_info('free_vm_public_key') if conn else None
    relay_key=conn.get_extra_info('free_vm_relay_key') if conn else None
    peer=process.get_extra_info('peername')
    if not peer or not isinstance(peer[0],str) or process.subsystem is not None:
        process.stderr.write(b'Invalid authentication.\r\n');process.exit(1);return
    if relay_key:
        try:
            if not settings or not isinstance(process.command,str) or not process.command.startswith('gap-relay-v1 '):
                raise GatewayError('relay_ticket_invalid')
            encoded=process.command[len('gap-relay-v1 '):]
            if len(encoded)>8192:
                raise GatewayError('relay_ticket_invalid')
            payload=json.loads(base64.urlsafe_b64decode(encoded+'='*(-len(encoded)%4)))
            if set(payload)!={'ticket','ssh_key','source_ip','command'} or not isinstance(payload['ssh_key'],str):
                raise GatewayError('relay_ticket_invalid')
            if payload['command'] is not None and (not isinstance(payload['command'],str) or len(payload['command'])>8192):
                raise GatewayError('relay_ticket_invalid')
            claims=verify_relay_ticket(payload['ticket'],payload['ssh_key'],relay_key,
                                       payload['source_ip'],settings)
        except Exception:
            process.stderr.write(b'Invalid fleet relay authorization.\r\n');process.exit(1);return
        await local_shell(runner,process,payload['ssh_key'],payload['source_ip'],
                          payload['command'],claims['project'])
        return
    if not key:
        process.stderr.write(b'Invalid authentication.\r\n');process.exit(1);return
    if settings:
        try:
            routing=await asyncio.to_thread(request_node,runner,'/internal/free-vm/route',
                {'ssh_key':key,'relay_key':settings['relay_public_key'],'source_ip':peer[0]})
            if routing.get('found') is True and routing.get('node_id')!=settings['node_id']:
                await relay_shell(process,key,peer[0],process.command,routing,settings)
                return
            if routing.get('found') not in (True,False):
                raise GatewayError('fleet_route_unavailable')
        except Exception as error:
            print('free_vm_route_error:',type(error).__name__,str(error)[:160],flush=True)
            process.stderr.write(b'Your VM route is temporarily unavailable. Please try again.\r\n')
            process.exit(1);return
    await local_shell(runner,process,key,peer[0],process.command)


async def serve(runner,port,host_key,settings=None):
    server=await asyncssh.listen('0.0.0.0',port,server_factory=FreeServer,
        server_host_keys=[str(host_key)],process_factory=lambda process:shell(runner,process,settings),
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
    node_id=settings.get('node_id')
    peers=settings.get('peers')
    public=settings.get('fleet_public_key')
    if (not isinstance(node_id,str) or not re.fullmatch(r'node-0[1-3]',node_id)
            or not isinstance(peers,dict) or set(peers)!={'node-01','node-02','node-03'}-{node_id}
            or not isinstance(public,str) or not re.fullmatch(r'[0-9a-f]{64}',public)):
        raise ValueError('invalid_free_vm_fleet_route_configuration')
    for peer in peers.values():
        if (not isinstance(peer,dict) or not {'host','host_key'}<=set(peer)<={'host','host_key','port'}
                or not isinstance(peer['host'],str)
                or not re.fullmatch(r'[a-z0-9.-]{1,253}',peer['host'])
                or not isinstance(peer['host_key'],str)
                or not re.fullmatch(r'ssh-ed25519 [A-Za-z0-9+/=]{60,128}',peer['host_key'])
                or type(peer.get('port',2121)) is not int or not 1<=peer.get('port',2121)<=65535):
            raise ValueError('invalid_free_vm_fleet_peer')
    relay_key=Path(config['state_dir'])/'free-vm-relay-key'
    if not relay_key.exists():
        pending=relay_key.with_suffix('.new')
        pending.write_bytes(asyncssh.generate_private_key('ssh-ed25519').export_private_key())
        os.chmod(pending,0o600);pending.replace(relay_key)
    elif relay_key.stat().st_mode & 0o077:
        raise ValueError('insecure_free_vm_relay_key')
    relay_public=asyncssh.read_private_key(str(relay_key)).export_public_key().decode().strip()
    route_settings=dict(settings,relay_key_file=relay_key,relay_public_key=relay_public)
    thread=threading.Thread(target=lambda:asyncio.run(serve(runner,port,key,route_settings)),daemon=True)
    thread.start()
    return thread
