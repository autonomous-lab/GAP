import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from ingress import Ingress
from microvm import MicroVMs, VMError

PROJECT = 'prj_' + 'a' * 24
OWNER = 'did:gap:' + 'b' * 64
VM = 'vm_' + 'c' * 32


class IngressTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.manager = MicroVMs({'state_dir': str(root), 'image_dir': str(root)}, None)
        self.meta = {'vm_id': VM, 'project_id': PROJECT, 'owner_did': OWNER, 'state': 'running',
                     'ports': [{'guest_port': 8000, 'worker_port': 23000}], 'vcpus': 1,
                     'memory_mib': 1024, 'disk_gib': 4}
        sync = patch.object(self.manager, 'sync_environment')
        sync.start()
        self.addCleanup(sync.stop)
        self.manager.save(self.meta)
        self.config = {'dedicated_caddy': True, 'public_url': 'https://gap.geta.team'}
        self.opener = patch('ingress.AdminConnection').start()
        self.addCleanup(patch.stopall)
        self.opener.return_value.getresponse.return_value.status = 200
        self.public = patch.object(self.manager, 'public', return_value={'state': 'running'}).start()
        self.ingress = Ingress(self.config, self.manager)

    def test_only_project_path_and_configured_guest_port(self):
        for extra in ({'hostname': 'other.test'}, {'upstream': '127.0.0.1:8080'}, {'guest_port': 22}, {'guest_port': 9000}):
            with self.assertRaises(VMError):
                self.ingress.perform(PROJECT, OWNER, {'vm_id': VM, 'enabled': True, 'guest_port': 8000, **extra})
        result = self.ingress.perform(PROJECT, OWNER, {'vm_id': VM, 'enabled': True, 'guest_port': 8000})
        self.assertTrue(result['ingress']['routed'])
        config, _ = self.ingress.configuration()
        route = config['apps']['http']['servers']['compose']['routes'][1]
        self.assertEqual(route['match'], [{'path': ['/apps/' + PROJECT + '/*']}])
        self.assertEqual(result['ingress']['url'], 'https://gap.geta.team/apps/' + PROJECT + '/')
        self.assertEqual(route['handle'][0]['strip_path_prefix'], '/apps/' + PROJECT)
        self.assertEqual(route['handle'][1]['upstreams'], [{'dial': '127.0.0.1:23000'}])
        self.assertTrue(config['admin']['listen'].startswith('unix/'))
        self.assertNotIn('tls', config['apps'])  # normal automatic public TLS

    def test_owner_and_vm_generation_are_enforced(self):
        for owner, vm in (('wrong-owner', VM), (OWNER, 'vm_' + 'd' * 32)):
            with self.assertRaises(VMError):
                self.ingress.perform(PROJECT, owner, {'vm_id': vm, 'enabled': True, 'guest_port': 8000})

    def test_stopped_destroyed_and_disabled_vms_have_no_routes(self):
        self.ingress.perform(PROJECT, OWNER, {'vm_id': VM, 'enabled': True, 'guest_port': 8000})
        self.public.return_value = {'state': 'stopped'}
        self.assertEqual(self.ingress.configuration()[1], set())
        self.public.return_value = {'state': 'running'}
        self.ingress.perform(PROJECT, OWNER, {'vm_id': VM, 'enabled': False})
        self.assertEqual(self.ingress.configuration()[1], set())

    def test_caddy_failure_prevents_port_reuse(self):
        self.opener.return_value.request.side_effect = OSError('offline')
        with patch.object(self.manager, 'perform') as mutate:
            with self.assertRaisesRegex(VMError, 'ingress_apply_failed'):
                self.ingress.vm_operation(PROJECT, OWNER, 'vm/destroy', {'vm_id': VM})
            mutate.assert_not_called()

    def test_routes_withdrawn_before_vm_mutation_and_reconciled_after_failure(self):
        events = []
        with patch.object(self.ingress, 'sync', side_effect=lambda exclude=None: events.append(exclude)), \
             patch.object(self.manager, 'perform', side_effect=VMError('failed')):
            with self.assertRaises(VMError):
                self.ingress.vm_operation(PROJECT, OWNER, 'vm/stop', {'vm_id': VM})
        self.assertEqual(events, [VM, None])

    def test_dedicated_unix_caddy_required(self):
        for change in ({'dedicated_caddy': False}, {'admin_url': 'http://public.example:2019'},
                       {'base_domain': '*.example.com'}, {'base_domain': 'example.com/path'},
                       {'http_port': 0}):
            with self.assertRaises(ValueError):
                Ingress({**self.config, **change}, self.manager)


if __name__ == '__main__':
    unittest.main()
