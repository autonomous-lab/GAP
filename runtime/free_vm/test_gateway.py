import asyncio
import base64
import io
import json
from pathlib import Path
import sys
import tempfile
import types
import unittest
from unittest.mock import Mock, patch
import urllib.error
import time

import asyncssh
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization

from gateway import (FreeServer, GatewayError, admission_error, claimable_banner, claimed_banner,
                     claimed_status, format_credits, gateway_error_message, interactive_shell_command, prepare,
                     request_node, retry_mutation, rpc_wait, shell, stable_id, welcome_banner,
                     key_hash, verify_relay_ticket, _used_relay_nonces, credit_block_reason)


class GatewayTests(unittest.TestCase):
    def test_known_rsa_key_relays_to_same_claimed_guest(self):
        async def run():
            home_host=asyncssh.generate_private_key('ssh-ed25519')
            ingress_host=asyncssh.generate_private_key('ssh-ed25519')
            guest_host=asyncssh.generate_private_key('ssh-ed25519')
            relay_key=asyncssh.generate_private_key('ssh-ed25519')
            guest_key=asyncssh.generate_private_key('ssh-ed25519')
            user_key=asyncssh.generate_private_key('ssh-rsa',key_size=2048)
            async def guest_handler(process):
                process.stdout.write(b'same-home-vm\n')
                process.exit(0)
            guest=await asyncssh.listen('127.0.0.1',0,server_host_keys=[guest_host],
                authorized_client_keys=asyncssh.import_authorized_keys(guest_key.export_public_key().decode()),
                process_factory=guest_handler,encoding=None)
            with tempfile.TemporaryDirectory() as directory:
                guest_path=Path(directory)/'guest-key'
                relay_path=Path(directory)/'relay-key'
                guest_path.write_bytes(guest_key.export_private_key())
                relay_path.write_bytes(relay_key.export_private_key())
                details={'status':'claimed','project_id':'prj_'+'a'*24,
                         'ssh_port':guest.get_port(),'guest_key':str(guest_path),
                         'guest_host_key':guest_host.export_public_key().decode().strip(),
                         'manage_url':'https://gap.geta.team/account',
                         'vcpus':1,'memory_mib':1024,'disk_gib':8,'app_ports':[8080]}
                home_settings={'node_id':'node-02','peers':{'node-01':{}},'fleet_public_key':'0'*64}
                with (patch('gateway.prepare',return_value=details) as prepare,
                      patch('gateway.verify_relay_ticket',return_value={'project':details['project_id']})):
                    home=await asyncssh.listen('127.0.0.1',0,server_factory=FreeServer,
                        server_host_keys=[home_host],
                        process_factory=lambda process:shell(None,process,home_settings),encoding=None)
                    ingress_settings={'node_id':'node-01','relay_key_file':relay_path,
                        'relay_public_key':relay_key.export_public_key().decode().strip(),
                        'peers':{'node-02':{'host':'127.0.0.1','port':home.get_port(),
                            'host_key':home_host.export_public_key().decode().strip()}}}
                    with patch('gateway.request_node',return_value={'found':True,'node_id':'node-02','ticket':'test'}):
                        ingress=await asyncssh.listen('127.0.0.1',0,server_factory=FreeServer,
                            server_host_keys=[ingress_host],
                            process_factory=lambda process:shell(None,process,ingress_settings),encoding=None)
                        try:
                            async with asyncssh.connect('127.0.0.1',port=ingress.get_port(),username='free',
                                    client_keys=[user_key],known_hosts=None,encoding=None) as connection:
                                result=await connection.run('true',check=False)
                            self.assertEqual(result.exit_status,0,repr((result.stdout,result.stderr)))
                            self.assertIn(b'YOUR MICROVM',result.stdout)
                            self.assertIn(b'same-home-vm',result.stdout)
                            prepare.assert_called_once()
                            self.assertEqual(prepare.call_args.args[1],user_key.export_public_key().decode().strip())
                        finally:
                            ingress.close();await ingress.wait_closed()
                    home.close();await home.wait_closed()
            guest.close();await guest.wait_closed()
        asyncio.run(run())

    def test_fleet_relay_ticket_is_signed_scoped_and_one_use(self):
        signer=Ed25519PrivateKey.generate()
        public=signer.public_key().public_bytes(serialization.Encoding.Raw,
                                                serialization.PublicFormat.Raw).hex()
        user=asyncssh.generate_private_key('ssh-rsa',key_size=2048).export_public_key().decode().strip()
        relay=asyncssh.generate_private_key('ssh-ed25519').export_public_key().decode().strip()
        settings={'node_id':'node-02','peers':{'node-01':{}},'fleet_public_key':public}
        claims={'v':1,'home':'node-02','ingress':'node-01',
                'ssh':key_hash(user),'relay':key_hash(relay),'ip':'8.8.8.8',
                'project':'prj_'+'a'*24,'iat':int(time.time()),
                'exp':int(time.time())+45,'nonce':'a'*32}
        encoded=base64.urlsafe_b64encode(json.dumps(claims,sort_keys=True,
                        separators=(',',':')).encode()).decode().rstrip('=')
        signed='gapr1.'+encoded
        ticket=signed+'.'+base64.urlsafe_b64encode(signer.sign(signed.encode())).decode().rstrip('=')
        _used_relay_nonces.clear()
        self.assertEqual(verify_relay_ticket(ticket,user,relay,'8.8.8.8',settings)['project'],claims['project'])
        with self.assertRaisesRegex(GatewayError,'relay_ticket_invalid'):
            verify_relay_ticket(ticket,user,relay,'8.8.8.8',settings)
        _used_relay_nonces.clear()
        with self.assertRaisesRegex(GatewayError,'relay_ticket_invalid'):
            verify_relay_ticket(ticket,user,relay,'8.8.8.9',settings)
        with self.assertRaisesRegex(GatewayError,'relay_ticket_invalid'):
            verify_relay_ticket(ticket,user,relay,'8.8.8.8',dict(settings,node_id='node-03'))

    def test_authentication_does_not_clutter_successful_terminal_sessions(self):
        server=FreeServer()
        connection=Mock()
        server.connection_made(connection)
        self.assertTrue(server.begin_auth('free'))
        self.assertTrue(server.begin_auth('wrong'))
        connection.send_auth_banner.assert_not_called()

    def test_transient_guest_readiness_is_retried_with_new_job_id(self):
        with (patch('gateway.rpc_wait',side_effect=[GatewayError('vm_operation_failed'),{'ok':True}]) as rpc,
              patch('gateway.time.sleep')):
            result=retry_mutation(None,'prj_'+'a'*24,'did:gap:'+'b'*64,'ingress','PUT',{'vm_id':'vm_'+'c'*32})
        self.assertEqual(result,{'ok':True})
        self.assertNotEqual(rpc.call_args_list[0].args[5]['request_id'],rpc.call_args_list[1].args[5]['request_id'])

    def test_claimed_vm_credit_failure_is_explained_without_retries(self):
        runner=Mock()
        runner.rpc.side_effect=[(200,{'status':'failed','result':{'error':'microvm_credits_or_budget_exhausted'}}),
                                (200,{'alerts':['credits_exhausted']})]
        with self.assertRaisesRegex(GatewayError,'claimed_vm_no_credits'):
            rpc_wait(runner,'prj_'+'a'*24,'did:gap:'+'b'*64,'vm/start','POST',{})
        with (patch('gateway.rpc_wait',side_effect=GatewayError('claimed_vm_no_credits')) as rpc,
              patch('gateway.time.sleep') as sleep):
            with self.assertRaisesRegex(GatewayError,'claimed_vm_no_credits'):
                retry_mutation(runner,'prj_'+'a'*24,'did:gap:'+'b'*64,'terminal/prepare','POST',{})
        rpc.assert_called_once()
        sleep.assert_not_called()
        for blocked in ('claimed_vm_budget_exhausted','claimed_vm_funding_blocked'):
            with (patch('gateway.rpc_wait',side_effect=GatewayError(blocked)) as rpc,
                  patch('gateway.time.sleep') as sleep):
                with self.assertRaisesRegex(GatewayError,blocked):
                    retry_mutation(runner,'prj_'+'a'*24,'did:gap:'+'b'*64,'terminal/prepare','POST',{})
            rpc.assert_called_once()
            sleep.assert_not_called()
        self.assertIn('https://gap.geta.team/account#billing',
                      gateway_error_message(GatewayError('claimed_vm_no_credits')))

    def test_credit_denial_distinguishes_project_budget_from_wallet(self):
        runner=Mock()
        runner.rpc.return_value=(200,{'alerts':['budget_exhausted'],
                                      'fleet':{'funding_status':'available'}})
        self.assertEqual(credit_block_reason(runner,'prj_'+'a'*24,'did:gap:'+'b'*64),
                         'claimed_vm_budget_exhausted')
        self.assertIn('/account#machines',gateway_error_message(GatewayError('claimed_vm_budget_exhausted')))
        runner.rpc.return_value=(200,{'alerts':[], 'fleet':{'funding_status':'exhausted'}})
        self.assertEqual(credit_block_reason(runner,'prj_'+'a'*24,'did:gap:'+'b'*64),
                         'claimed_vm_no_credits')
        runner.rpc.side_effect=RuntimeError('ledger unavailable')
        self.assertEqual(credit_block_reason(runner,'prj_'+'a'*24,'did:gap:'+'b'*64),
                         'claimed_vm_funding_blocked')

    def test_request_ids_are_stable_and_operation_scoped(self):
        project='prj_'+'a'*24
        self.assertEqual(stable_id(project,'create'),stable_id(project,'create'))
        self.assertNotEqual(stable_id(project,'create'),stable_id(project,'terminal'))

    def test_interactive_shell_displays_the_live_deadline_at_each_prompt(self):
        command=interactive_shell_command(1234567890)
        self.assertIn('PROMPT_COMMAND=',command)
        self.assertIn('1234567890 - $(date +%s)',command)
        self.assertIn('bash --login -i',command)
        self.assertIn('PS1=',command)
        self.assertIn('left',command)
        self.assertNotIn('restantes',command)

    def test_welcome_panel_is_english_and_only_colored_for_terminals(self):
        details={'preview':{'preview_url':'https://example.invalid/apps/demo',
                            'username':'free','password':'unique-password'},
                 'claim_url':'https://example.invalid/free-vm/claim#private-token'}
        plain=welcome_banner(details,3542)
        self.assertIn('59m 02s left',plain)
        self.assertIn('1 vCPU · 1 GiB RAM · 8 GiB disk',plain)
        self.assertIn('Preview:     https://example.invalid/apps/demo',plain)
        self.assertIn('Your SSH IP is allowed automatically.',plain)
        self.assertNotIn('unique-password',plain)
        self.assertIn('Claim:       https://example.invalid/free-vm/claim#private-token',plain)
        self.assertNotIn('\x1b[',plain)
        colored=welcome_banner(details,3542,True)
        self.assertIn('\x1b[1;36m',colored)
        self.assertIn('59m 02s left',colored)
        ready=welcome_banner(dict(details,guest_image='free-vm-v3'),3542,ready_seconds=2)
        self.assertIn('Ready in 2s',ready)
        self.assertIn('Ctrl+B, then D',ready)

    def test_claimable_key_recovers_link_without_provisioning_a_guest(self):
        details={'status':'claimable','claim_url':'https://example.invalid/claim#private-token',
                 'claim_until':90100}
        with patch('gateway.request_node',return_value=details):
            self.assertEqual(prepare(object(),'ssh-ed25519 test','203.0.113.1'),details)
        panel=claimable_banner(details,3700)
        self.assertIn('TRIAL COMPLETE',panel)
        self.assertIn('24h 00m left to claim it',panel)
        self.assertIn('CLAIM        https://example.invalid/claim#private-token',panel)
        self.assertNotIn('BASIC AUTH',panel)
        self.assertNotIn('\x1b[',panel)
        self.assertIn('\x1b[1;36m',claimable_banner(details,3700,True))

    def test_claimed_key_prepares_existing_vm_without_new_trial(self):
        project='prj_'+'a'*24
        owner='did:gap:'+'b'*64
        meta={'vm_id':'vm_'+'c'*32,'state':'stopped','tier':'trial',
              'guest_image':'free-vm-v2','ssh_port':2200,'vcpus':1.5,
              'memory_mib':1536,'disk_gib':12,
              'ports':[{'guest_port':8080,'worker_port':2201}]}
        manager=Mock()
        manager.read.side_effect=[meta,dict(meta,state='running')]
        with tempfile.TemporaryDirectory() as directory:
            folder=Path(directory)
            (folder/'seed').mkdir()
            (folder/'terminal_key').write_text('test')
            (folder/'seed'/'ssh_host_ed25519_key.pub').write_text('ssh-ed25519 test-key')
            manager.folder.return_value=folder
            runner=Mock(hypervisor=manager)
            admission={'status':'claimed','project_id':project,'owner_did':owner}
            with (patch('gateway.request_node',return_value=admission),
                  patch('gateway.rpc_wait',return_value={'ok':True}) as rpc,
                  patch('gateway.retry_mutation',return_value={'ok':True}) as terminal):
                result=prepare(runner,'ssh-ed25519 test','203.0.113.1')
            self.assertEqual(result['status'],'claimed')
            self.assertEqual(result['vm_id'],meta['vm_id'])
            self.assertEqual(result['app_ports'],[8080])
            self.assertNotIn('active_until',result)
            self.assertEqual(rpc.call_args.args[3],'vm/start')
            self.assertEqual(terminal.call_args.args[3],'terminal/prepare')
            self.assertIn('YOUR MICROVM',claimed_banner(result))
            self.assertIn('1.5 vCPU  ·  1536 MiB RAM  ·  12 GiB disk',claimed_banner(result))
            self.assertIn('APP PORTS   8080 (inside VM)',claimed_banner(result))
            self.assertIn('PUBLIC URL  not configured',claimed_banner(result))
            self.assertIn('PORT MAP    no direct TCP/UDP ports',claimed_banner(result))
            self.assertIn('CREDITS     unavailable; see dashboard',claimed_banner(result))
            self.assertIn('SSH ENTRY   port 2121',claimed_banner(result))
            self.assertNotIn('CLAIM',claimed_banner(result))

    def test_claimed_banner_uses_public_routes_and_shared_wallet_not_local_reserve(self):
        project='prj_'+'a'*24
        owner='did:gap:'+'b'*64
        meta={'vm_id':'vm_'+'c'*32}
        network=Mock()
        network.public.return_value={'hostname':'vm.example.test','ports':[
            {'public_port':23111,'guest_port':8080,'protocol':'tcp','routed':True},
            {'public_port':23112,'guest_port':22,'protocol':'tcp','routed':False},
            {'public_port':23113,'guest_port':None,'protocol':None,'routed':False}]}
        ingress=Mock()
        ingress.public.return_value={'enabled':True,'routed':True,'guest_port':8080,
                                     'url':'https://vm.example.test/apps/demo/'}
        ledger=Mock(projects={project})
        ledger.transport.return_value={'project_id':project,
            'total_remaining_microcredits':125_250_000,'microcredits_per_credit':1_000_000}
        runner=types.SimpleNamespace(ingress=ingress,hypervisor=types.SimpleNamespace(network=network),
                                     runtime=types.SimpleNamespace(ledger=ledger),
                                     rpc=Mock(return_value=(200,{'budget_microcredits':20_000_000,
                                                                  'budget_spent_microcredits':3_500_000,
                                                                  'balance_microcredits':500_000})))
        details=claimed_status(runner,project,owner,meta)
        self.assertEqual(details['wallet_microcredits'],125_250_000)
        ledger.transport.assert_called_once_with({'action':'wallet-status',
            'project_id':project,'owner_did':owner})
        panel=claimed_banner(dict(details,vcpus=1,memory_mib=1024,disk_gib=8,
                                  app_ports=[8080],manage_url='https://gap.geta.team/account'))
        self.assertIn('PUBLIC URL  https://vm.example.test/apps/demo/',panel)
        self.assertIn('HTTPS MAP   public HTTPS → 8080/tcp (inside VM)',panel)
        self.assertIn('PORT MAP    vm.example.test:23111 → 8080/tcp',panel)
        self.assertIn('PORT MAP    vm.example.test:23112 → 22/tcp (pending)',panel)
        self.assertNotIn('23113',panel)
        self.assertIn('CREDITS     125.25 remaining (shared account)',panel)
        self.assertIn('BUDGET LEFT 16.5 credits (this project)',panel)
        self.assertNotIn('0.5 remaining',panel)
        self.assertEqual(format_credits(1), '0.000001')

    def test_claimed_status_wallet_outage_never_looks_like_zero(self):
        ledger=Mock(projects={'prj_'+'a'*24})
        ledger.transport.side_effect=RuntimeError('offline')
        runner=types.SimpleNamespace(ingress=None,hypervisor=types.SimpleNamespace(network=None),
                                     runtime=types.SimpleNamespace(ledger=ledger),
                                     rpc=Mock(side_effect=RuntimeError('offline')))
        details=claimed_status(runner,'prj_'+'a'*24,'did:gap:'+'b'*64,{})
        self.assertIsNone(details['wallet_microcredits'])
        panel=claimed_banner(dict(details,vcpus=1,memory_mib=1024,disk_gib=8,
                                  app_ports=[],manage_url='https://gap.geta.team/account'))
        self.assertIn('CREDITS     unavailable; see dashboard',panel)
        self.assertNotIn('BUDGET LEFT',panel)

    def test_admission_errors_do_not_pretend_every_failure_is_capacity(self):
        self.assertEqual(admission_error('anonymous trial capacity reached'),'capacity_reached')
        self.assertEqual(admission_error('anonymous trial limit reached'),'trial_already_reserved')
        self.assertEqual(admission_error('fleet admission rejected the anonymous VM: free_vm_ip_already_reserved'),
                         'trial_already_reserved')
        self.assertEqual(admission_error('fleet admission rejected the anonymous VM: free_vm_expired_pending_cleanup'),
                         'cleanup_pending')
        self.assertIn('cleanup',gateway_error_message(GatewayError('cleanup_pending')))
        self.assertNotIn('capacity',gateway_error_message(GatewayError('admission_or_node_unavailable')))

    def test_node_http_error_is_classified_without_exposing_internal_body(self):
        runner_module=types.ModuleType('runner')
        runner_module.load_config=lambda path:{'node_url':'http://127.0.0.1:8080'}
        runner_module.NoRedirect=type('NoRedirect',(),{})
        error=urllib.error.HTTPError('http://127.0.0.1:8080',400,'Bad request',{},
            io.BytesIO(b'{"error":{"code":"invalid_request","message":"anonymous trial limit reached"}}'))
        opener=Mock()
        opener.open.side_effect=error
        with patch.dict(sys.modules,{'runner':runner_module}),patch('gateway.urllib.request.build_opener',return_value=opener):
            with self.assertRaisesRegex(GatewayError,'trial_already_reserved'):
                request_node(Mock(path='runner.json',token='test-token'),'/internal/free-vm/reserve',{})

    def test_claimable_ssh_session_returns_link_and_no_shell(self):
        async def run():
            host=asyncssh.generate_private_key('ssh-ed25519')
            client=asyncssh.generate_private_key('ssh-ed25519')
            details={'status':'claimable','claim_url':'https://example.invalid/claim#private-token',
                     'claim_until':int(__import__('time').time())+3600}
            with patch('gateway.prepare',return_value=details):
                server=await asyncssh.listen('127.0.0.1',0,server_factory=FreeServer,
                    server_host_keys=[host],process_factory=lambda process:shell(None,process),encoding=None)
                try:
                    async with asyncssh.connect('127.0.0.1',port=server.get_port(),username='free',
                            client_keys=[client],known_hosts=None,encoding=None) as connection:
                        command=await connection.run('true',check=False)
                        self.assertEqual(command.exit_status,1)
                        self.assertIn(b'TRIAL COMPLETE',command.stdout)
                        session=await connection.create_process(term_type='xterm',encoding=None)
                        result=await session.wait()
                        self.assertIn(result.exit_status,(None,0))
                        self.assertIn(b'TRIAL COMPLETE',result.stdout)
                        self.assertIn(b'CLAIM        https://example.invalid/claim#private-token',result.stdout)
                        self.assertNotIn(b'Your workspace is ready',result.stdout)
                finally:
                    server.close();await server.wait_closed()
        asyncio.run(run())

    def test_ssh_requires_a_valid_ed25519_signature_before_session(self):
        async def run():
            sessions=[]
            async def handler(process):
                connection=process.get_extra_info('connection')
                sessions.append((connection.get_extra_info('free_vm_public_key'),process.get_extra_info('peername')))
                process.stdout.write('authenticated\n')
                process.exit(0)
            host=asyncssh.generate_private_key('ssh-ed25519')
            client=asyncssh.generate_private_key('ssh-ed25519')
            rsa=asyncssh.generate_private_key('ssh-rsa',key_size=2048)
            weak_rsa=asyncssh.generate_private_key('ssh-rsa',key_size=1024)
            server=await asyncssh.listen('127.0.0.1',0,server_factory=FreeServer,
                server_host_keys=[host],process_factory=handler,
                signature_algs=['ssh-ed25519','rsa-sha2-512','rsa-sha2-256'])
            port=server.get_port()
            try:
                with self.assertRaises(asyncssh.PermissionDenied):
                    await asyncssh.connect('127.0.0.1',port=port,username='wrong',
                        client_keys=[client],known_hosts=None,password_auth=False)
                self.assertEqual(sessions,[])
                with self.assertRaises(asyncssh.PermissionDenied):
                    await asyncssh.connect('127.0.0.1',port=port,username='free',
                        client_keys=[weak_rsa],known_hosts=None,password_auth=False)
                async with asyncssh.connect('127.0.0.1',port=port,username='free',
                        client_keys=[client],known_hosts=None,password_auth=False) as connection:
                    result=await connection.run('true')
                    self.assertEqual(result.stdout,'authenticated\n')
                async with asyncssh.connect('127.0.0.1',port=port,username='free',
                        client_keys=[rsa],known_hosts=None,password_auth=False) as connection:
                    result=await connection.run('true')
                    self.assertEqual(result.stdout,'authenticated\n')
                self.assertEqual(len(sessions),2)
                self.assertTrue(sessions[0][0].startswith('ssh-ed25519 '))
                self.assertEqual(sessions[0][1][0],'127.0.0.1')
                self.assertTrue(sessions[1][0].startswith('ssh-rsa '))
            finally:
                server.close();await server.wait_closed()
        asyncio.run(run())

    def test_authenticated_session_bridges_to_the_guest_with_pinned_host_key(self):
        async def run():
            gateway_host=asyncssh.generate_private_key('ssh-ed25519')
            guest_host=asyncssh.generate_private_key('ssh-ed25519')
            user_key=asyncssh.generate_private_key('ssh-ed25519')
            terminal_key=asyncssh.generate_private_key('ssh-ed25519')
            guest_commands=[]
            async def guest_handler(process):
                guest_commands.append(process.command)
                process.stdout.write(b'from-guest\n')
                await process.stdout.drain()
                process.exit(0)
            guest=await asyncssh.listen('127.0.0.1',0,server_host_keys=[guest_host],
                process_factory=guest_handler,authorized_client_keys=asyncssh.import_authorized_keys(terminal_key.export_public_key().decode()),
                encoding=None)
            with tempfile.TemporaryDirectory() as directory:
                key_path=Path(directory)/'terminal_key'
                key_path.write_bytes(terminal_key.export_private_key())
                details={'active_until':__import__('time').time()+60,'ssh_port':guest.get_port(),
                    'guest_key':str(key_path),'guest_host_key':guest_host.export_public_key().decode().strip(),
                    'preview':{'preview_url':'https://test.invalid','username':'u','password':'p'},
                    'claim_url':'https://test.invalid/claim#token'}
                async with asyncssh.connect('127.0.0.1',port=guest.get_port(),username='root',
                        client_keys=[str(key_path)],known_hosts=None,encoding=None) as direct:
                    direct_result=await direct.run('true')
                    self.assertEqual(direct_result.stdout,b'from-guest\n',repr(direct_result))
                with patch('gateway.prepare',return_value=details):
                    gateway=await asyncssh.listen('127.0.0.1',0,server_factory=FreeServer,
                        server_host_keys=[gateway_host],process_factory=lambda process:shell(None,process),encoding=None)
                    try:
                        async with asyncssh.connect('127.0.0.1',port=gateway.get_port(),username='free',
                                client_keys=[user_key],known_hosts=None,encoding=None) as connection:
                            result=await connection.run('true')
                            self.assertEqual(result.exit_status,0,repr((result.stdout,result.stderr)))
                            self.assertIn(b'Welcome to your GAP MicroVM',result.stdout)
                            self.assertIn(b'Preview:     https://test.invalid',result.stdout)
                            self.assertIn(b'Your SSH IP is allowed automatically.',result.stdout)
                            self.assertNotIn(b'Basic Auth:',result.stdout)
                            self.assertIn(b'Claim:       https://test.invalid/claim#token',result.stdout)
                            self.assertIn(b'from-guest\n',result.stdout)
                            interactive=await connection.create_process(term_type='xterm',encoding=None)
                            welcome=await interactive.wait()
                            self.assertIn(b'Welcome to your GAP MicroVM',welcome.stdout)
                            self.assertIn(b'\x1b[1;36m',welcome.stdout)
                            self.assertIn(b'from-guest\n',welcome.stdout)
                            self.assertEqual(guest_commands[0],'true')
                            self.assertIn('PROMPT_COMMAND=',guest_commands[-1])
                        claimed=dict(details,status='claimed',manage_url='https://gap.geta.team/account#machines',
                                     vcpus=1,memory_mib=1024,disk_gib=8,app_ports=[8080])
                        claimed.pop('active_until')
                        with patch('gateway.prepare',return_value=claimed):
                            async with asyncssh.connect('127.0.0.1',port=gateway.get_port(),username='free',
                                    client_keys=[user_key],known_hosts=None,encoding=None) as connection:
                                result=await connection.run('true')
                                self.assertEqual(result.exit_status,0)
                                self.assertIn(b'YOUR MICROVM',result.stdout)
                                self.assertIn(b'1 vCPU  \xc2\xb7  1 GiB RAM  \xc2\xb7  8 GiB disk',result.stdout)
                                self.assertIn(b'APP PORTS   8080 (inside VM)',result.stdout)
                                self.assertIn(b'from-guest\n',result.stdout)
                                self.assertNotIn(b'FREE MICROVM',result.stdout)
                    finally:
                        gateway.close();await gateway.wait_closed()
            guest.close();await guest.wait_closed()
        asyncio.run(run())

    def test_claimed_command_forwards_stdin_eof_and_finishes(self):
        async def run():
            gateway_host=asyncssh.generate_private_key('ssh-ed25519')
            guest_host=asyncssh.generate_private_key('ssh-ed25519')
            user_key=asyncssh.generate_private_key('ssh-ed25519')
            terminal_key=asyncssh.generate_private_key('ssh-ed25519')
            async def guest_handler(process):
                await process.stdin.read()
                process.stdout.write(b'input-finished\n')
                process.exit(0)
            guest=await asyncssh.listen('127.0.0.1',0,server_host_keys=[guest_host],
                process_factory=guest_handler,authorized_client_keys=asyncssh.import_authorized_keys(terminal_key.export_public_key().decode()),encoding=None)
            with tempfile.TemporaryDirectory() as directory:
                key_path=Path(directory)/'terminal_key'
                key_path.write_bytes(terminal_key.export_private_key())
                details={'status':'claimed','ssh_port':guest.get_port(),'guest_key':str(key_path),
                    'guest_host_key':guest_host.export_public_key().decode().strip(),
                    'manage_url':'https://gap.geta.team/account',
                    'vcpus':1,'memory_mib':1024,'disk_gib':8,'app_ports':[8080]}
                with patch('gateway.prepare',return_value=details):
                    gateway=await asyncssh.listen('127.0.0.1',0,server_factory=FreeServer,
                        server_host_keys=[gateway_host],process_factory=lambda process:shell(None,process),encoding=None)
                    try:
                        async with asyncssh.connect('127.0.0.1',port=gateway.get_port(),username='free',
                                client_keys=[user_key],known_hosts=None,encoding=None) as connection:
                            process=await connection.create_process('true')
                            process.stdin.write_eof()
                            result=await asyncio.wait_for(process.wait(),5)
                            self.assertEqual(result.exit_status,0)
                            self.assertIn(b'input-finished',result.stdout)
                    finally:
                        gateway.close();await gateway.wait_closed()
            guest.close();await guest.wait_closed()
        asyncio.run(run())
