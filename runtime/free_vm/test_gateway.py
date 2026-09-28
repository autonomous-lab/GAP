import asyncio
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import asyncssh

from gateway import FreeServer, GatewayError, interactive_shell_command, retry_mutation, shell, stable_id


class GatewayTests(unittest.TestCase):
    def test_missing_key_gets_a_useful_authentication_hint(self):
        server=FreeServer()
        connection=Mock()
        server.connection_made(connection)
        self.assertTrue(server.begin_auth('free'))
        message=connection.send_auth_banner.call_args.args[0]
        self.assertIn('ssh-keygen -t ed25519',message)
        self.assertIn('ssh -i ~/.ssh/id_ed25519 -p 2121',message)
        connection.send_auth_banner.reset_mock()
        self.assertTrue(server.begin_auth('wrong'))
        connection.send_auth_banner.assert_not_called()

    def test_transient_guest_readiness_is_retried_with_new_job_id(self):
        with (patch('gateway.rpc_wait',side_effect=[GatewayError('vm_operation_failed'),{'ok':True}]) as rpc,
              patch('gateway.time.sleep')):
            result=retry_mutation(None,'prj_'+'a'*24,'did:gap:'+'b'*64,'ingress','PUT',{'vm_id':'vm_'+'c'*32})
        self.assertEqual(result,{'ok':True})
        self.assertNotEqual(rpc.call_args_list[0].args[5]['request_id'],rpc.call_args_list[1].args[5]['request_id'])

    def test_request_ids_are_stable_and_operation_scoped(self):
        project='prj_'+'a'*24
        self.assertEqual(stable_id(project,'create'),stable_id(project,'create'))
        self.assertNotEqual(stable_id(project,'create'),stable_id(project,'terminal'))

    def test_interactive_shell_displays_the_live_deadline_at_each_prompt(self):
        command=interactive_shell_command(1234567890)
        self.assertIn('PROMPT_COMMAND=',command)
        self.assertIn('1234567890 - $(date +%s)',command)
        self.assertIn('bash --login -i',command)
        self.assertIn('min %02d s restantes',command)

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
                            self.assertIn(b'GAP MicroVM',result.stdout)
                            self.assertIn(b'Preview : https://test.invalid',result.stdout)
                            self.assertIn(b'Basic Auth : u / p',result.stdout)
                            self.assertIn(b'R\xc3\xa9clamer : https://test.invalid/claim#token',result.stdout)
                            self.assertIn(b'from-guest\n',result.stdout)
                            interactive=await connection.create_process(term_type='xterm',encoding=None)
                            welcome=await interactive.wait()
                            self.assertIn(b'GAP MicroVM',welcome.stdout)
                            self.assertIn(b'from-guest\n',welcome.stdout)
                            self.assertEqual(guest_commands[0],'true')
                            self.assertIn('PROMPT_COMMAND=',guest_commands[-1])
                    finally:
                        gateway.close();await gateway.wait_closed()
            guest.close();await guest.wait_closed()
        asyncio.run(run())
