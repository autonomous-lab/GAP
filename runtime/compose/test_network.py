"""Port isolation, durable reservations, validation and failed application."""
import base64
import json
import time
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from microvm import MicroVMs, VMError
from network import keys, validate

P = 'prj_' + 'a' * 24
O = 'did:gap:' + 'b' * 64
V = 'vm_' + 'c' * 32
KEY = 'ssh-ed25519 ' + base64.b64encode(b'\0\0\0\x0bssh-ed25519\0\0\0\x20' + b'x' * 32).decode()


class NetworkTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.manager = MicroVMs({'state_dir': self.temp.name, 'image_dir': self.temp.name,
            'public_network': {'hostname': 'sites.gap.geta.team', 'first_port': 24000, 'last_port': 24009}}, None)
        sync = patch.object(self.manager, 'sync_environment')
        sync.start()
        self.addCleanup(sync.stop)
        self.net = self.manager.network
        self.meta = dict(project_id=P, owner_did=O, vm_id=V, state='stopped', vcpus=1,
                         memory_mib=1024, disk_gib=8, ports=[], ssh_port=33000)

    def reserve(self, meta):
        with self.manager.allocation_lock():
            self.net.allocate(meta)
            self.manager.save(meta)

    def test_exclusive_durable_five_numbers_and_release(self):
        self.reserve(self.meta)
        second = dict(self.meta, project_id='prj_'+'d'*24, vm_id='vm_'+'e'*32)
        self.reserve(second)
        self.assertEqual(len(self.meta['public_ports']), 5)
        self.assertFalse(set(self.meta['public_ports']) & set(second['public_ports']))
        with self.assertRaisesRegex(VMError, 'exhausted'):
            self.net.allocate(dict(self.meta))
        self.meta['state'] = 'destroyed'
        self.manager.save(self.meta)
        third = dict(self.meta, state='creating')
        self.net.allocate(third)
        self.assertEqual(third['public_ports'], self.meta['public_ports'])

    def test_slots_only_no_arbitrary_host_public_port_or_qmp(self):
        good = dict(request_id='a'*32, vm_id=V, mappings=[dict(slot=1, guest_port=22, protocol='tcp')])
        validate('ports', good)
        for item in [dict(slot=True, guest_port=22, protocol='tcp'), dict(slot=6, guest_port=22, protocol='tcp'),
                     dict(slot=1, guest_port=0, protocol='tcp'), dict(slot=1, guest_port=22, protocol='tcp;quit'),
                     dict(slot=1, guest_port=22, protocol='tcp', host='127.0.0.1')]:
            with self.assertRaises(VMError):
                validate('ports', dict(good, mappings=[item]))
        with self.assertRaises(VMError):
            validate('ports', dict(good, mappings=good['mappings']*2))

    def test_udp_reply_demultiplexing_requires_unique_guest_targets(self):
        body = dict(request_id='a'*32, vm_id=V, mappings=[dict(slot=1, guest_port=7000, protocol='both'), dict(slot=2, guest_port=7000, protocol='udp')])
        with self.assertRaisesRegex(VMError, 'duplicate_udp'):
            validate('ports', body)
        body['mappings'][1]['protocol'] = 'tcp'
        validate('ports', body)

    def test_key_options_private_keys_and_newlines_rejected(self):
        self.assertEqual(keys([KEY+' comment', KEY]), [KEY])
        for key in ['command="bad" '+KEY, KEY+'\n'+KEY, 'PRIVATE KEY', 'ssh-ed25519 YQ==']:
            with self.assertRaises(VMError):
                keys([key])

    def test_owner_generation_and_hot_replacement(self):
        self.reserve(self.meta)
        body = dict(request_id='a'*32, vm_id=V, mappings=[dict(slot=1, guest_port=22, protocol='both')])
        with self.assertRaisesRegex(VMError, 'owner'):
            self.net.perform(P, 'other', 'ports', body)
        with self.assertRaisesRegex(VMError, 'generation'):
            self.net.perform(P, O, 'ports', dict(body, vm_id='vm_'+'d'*32))
        with patch.object(self.manager, 'alive', return_value=True), patch.object(self.manager, 'qmp', side_effect=lambda m, c, *a: {'status': 'running'} if c == 'query-status' else ('host forwarding rule for ' + a[0]['command-line'].split()[-1] + ' removed\r\n' if a[0]['command-line'].startswith('hostfwd_remove') else '')) as qmp:
            self.net.perform(P, O, 'ports', body)
            commands = [c.args[2]['command-line'] for c in qmp.call_args_list if len(c.args)>2]
            self.assertEqual(sum(c.startswith('hostfwd_remove') for c in commands), 10)
            self.assertEqual(sum(c.startswith('hostfwd_add') for c in commands), 2)
        recovered = self.manager.read(P, O)
        self.assertEqual(recovered['public_ports'], self.meta['public_ports'])
        self.assertEqual(list(self.net.rules(recovered)), [f'tcp:0.0.0.0:{self.meta["public_ports"][0]}-:22', f'udp:0.0.0.0:{self.meta["public_ports"][0]}-:22'])
        self.net.perform(P, O, 'ports', dict(body, mappings=[]))
        self.assertEqual(self.manager.read(P, O)['public_mappings'], [])

    def test_runner_routes_jobs_and_rechecks_approval(self):
        from runner import Runner, Failure
        root = Path(self.temp.name)
        (root/'token').write_text('x'*40)
        path = root/'runner.json'
        path.write_text(json.dumps({'approved_only': True, 'token_file': str(root/'token'),
            'state_dir': str(root/'jobs'), 'node_url': 'http://127.0.0.1:1',
            'hypervisor': {'state_dir': self.temp.name, 'image_dir': self.temp.name,
                'public_network': {'hostname': 'sites.gap.geta.team', 'first_port': 24000, 'last_port': 24009}}}))
        runner = Runner(path)
        sync = patch.object(runner.hypervisor, 'sync_environment')
        sync.start()
        self.addCleanup(sync.stop)
        self.reserve(self.meta)
        rpc = dict(project_id=P, owner_did=O, method='PUT', action='ports',
                   body=dict(request_id='a'*32, vm_id=V, mappings=[dict(slot=1, guest_port=22, protocol='tcp')]))
        with patch.object(runner, 'authorize', return_value={'managed': True}) as authorize:
            status, job = runner.rpc(rpc)
            self.assertEqual(status, 202)
            deadline = time.monotonic()+3
            while time.monotonic()<deadline:
                _, result = runner.rpc(dict(rpc, method='GET', action='jobs/'+job['job_id']))
                if result['status'] not in ('queued','running'):
                    break
                time.sleep(.01)
            self.assertEqual(result['status'], 'succeeded')
            self.assertGreaterEqual(authorize.call_count, 3)
            _, view = runner.rpc(dict(rpc, method='GET'))
            self.assertEqual(view['ports'][0]['guest_port'], 22)
            _, replay = runner.rpc(rpc)
            self.assertEqual(replay['job_id'], job['job_id'])
            with self.assertRaisesRegex(Failure, 'conflict'):
                runner.rpc(dict(rpc, body=dict(rpc['body'], mappings=[])))
        with patch.object(runner, 'authorize', side_effect=Failure(403, 'revoked')):
            with self.assertRaisesRegex(Failure, 'revoked'):
                runner.rpc(dict(rpc, method='GET', action='ssh'))

    def test_failed_apply_remains_observable_and_retryable(self):
        self.reserve(self.meta)
        body = dict(request_id='a'*32, vm_id=V, mappings=[dict(slot=1, guest_port=22, protocol='tcp')])
        with patch.object(self.manager, 'alive', return_value=True), patch.object(self.net, 'apply', side_effect=VMError('fail')):
            with self.assertRaises(VMError):
                self.net.perform(P, O, 'ports', body)
        self.assertTrue(self.manager.read(P, O)['network_pending'])
        self.net.perform(P, O, 'ports', body)
        self.assertFalse(self.manager.read(P, O)['network_pending'])


if __name__ == '__main__':
    unittest.main()
