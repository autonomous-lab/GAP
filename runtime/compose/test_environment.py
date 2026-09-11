import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from environment import variables, install, validate
from microvm import MicroVMs, VMError


class EnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.meta = dict(project_id='prj_'+'a'*24, vm_id='vm_'+'b'*32, owner_did='did:gap:'+'c'*64,
                         state='stopped', ports=[dict(guest_port=8000,worker_port=33333)],
                         public_ports=[24000,24001,24002,24003,24004],
                         public_mappings=[dict(slot=1,guest_port=22,protocol='tcp'),dict(slot=2,guest_port=9000,protocol='both')],
                         ingress=dict(enabled=True, guest_port=8000))

    def test_guest_public_ports_and_http_origin_are_distinct_without_secrets(self):
        self.meta['secret_token'] = 'never-export'
        env = variables(self.meta, 'sites.gap.geta.team', 'https://gap.geta.team')
        self.assertEqual(env['GAP_HTTP_PORT'], '8000')
        self.assertEqual(env['GAP_PORT_2_PUBLIC'], '24001')
        self.assertEqual(env['GAP_PORT_2_GUEST'], '9000')
        self.assertEqual(env['GAP_PORT_2_PROTOCOL'], 'both')
        self.assertEqual(env['GAP_PORT_3_GUEST'], '')
        self.assertEqual(len(json.loads(env['GAP_PORTS_JSON'])), 5)
        self.assertTrue(env['GAP_WS_URL'].startswith('wss://'))
        self.assertNotIn('never-export', json.dumps(env))
        self.assertNotIn('33333', json.dumps(env))
        self.meta['ingress']['enabled'] = False
        disabled = variables(self.meta, 'sites.gap.geta.team', 'https://gap.geta.team')
        for key in ('GAP_HTTP_PORT','GAP_PUBLIC_URL','GAP_WS_URL'):
            self.assertEqual(disabled[key], '')
        self.assertNotEqual(env['GAP_ENV_REVISION'],disabled['GAP_ENV_REVISION'])

    def test_shell_escaping_and_formats_round_trip(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            values = variables(self.meta)
            values['GAP_TEST_LITERAL'] = "apostrophe' dollar$HOME `false` $(false) \"quote\""
            install(values, root, ssh_dir=root/'ssh')
            self.assertEqual((root/'ssh/environment').read_text(), (root/'runtime.env').read_text())
            code = '. "$1"; python3 -c "import os,json;print(json.dumps(dict(os.environ)))"'
            r = subprocess.run(['sh','-c',code,'test',str(root/'runtime.sh')],capture_output=True,text=True,check=True)
            env = json.loads(r.stdout)
            self.assertEqual(env['GAP_TEST_LITERAL'], values['GAP_TEST_LITERAL'])
            self.assertEqual(json.loads((root/'runtime.json').read_text()), values)
            self.assertEqual(dict(line.split('=',1) for line in (root/'runtime.env').read_text().splitlines()), values)
            self.assertEqual((root/'runtime.json').stat().st_mode & 0o777, 0o644)
            self.assertEqual(root.stat().st_mode & 0o777, 0o755)
        for invalid in ({'PATH':'bad'},{'GAP_TEST':'one\ntwo'},{'GAP_TEST':'\x00'}):
            with self.assertRaises(ValueError):
                validate(invalid)

    def test_failed_live_sync_is_visible_and_stopped_seed_is_refreshed(self):
        with tempfile.TemporaryDirectory() as temp:
            manager=MicroVMs(dict(state_dir=temp,image_dir=temp),None)
            seed=manager.folder(self.meta)/'seed';seed.mkdir(parents=True)
            manager.save(self.meta)
            with patch.object(manager,'alive',return_value=True), patch.object(manager,'guest',return_value={}), patch.object(manager,'execute_guest',return_value={'ok':False}):
                with self.assertRaisesRegex(VMError,'environment_update_failed'):
                    manager.sync_environment(self.meta)
            self.assertTrue(manager.read(self.meta['project_id'],self.meta['owner_did'])['environment_sync_pending'])
            manager.sync_environment(self.meta)
            self.assertFalse(manager.read(self.meta['project_id'],self.meta['owner_did'])['environment_sync_pending'])
            self.assertEqual(json.loads((seed/'runtime.json').read_text())['GAP_VM_ID'],self.meta['vm_id'])


if __name__ == '__main__':
    unittest.main()
