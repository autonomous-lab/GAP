import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('firewall',Path(__file__).with_name('configure-microvm-firewall.py'))
firewall=importlib.util.module_from_spec(spec)
spec.loader.exec_module(firewall)


class FirewallTests(unittest.TestCase):
    def test_enabled_gateway_survives_provider_firewall_reload_without_opening_control_ports(self):
        with tempfile.TemporaryDirectory() as directory:
            config=Path(directory)/'runner.json'
            config.write_text(json.dumps({'hypervisor':{'public_network':{'first_port':24000,'last_port':24099}},
                                          'free_vm':{'enabled':True}}))
            script=Path(directory)/'firewall.sh';script.write_text(firewall.MARKER+'\n')
            with patch.object(firewall.subprocess,'check_output',return_value='-N DOCKER-USER\n'),patch.object(firewall.subprocess,'run') as run:
                run.return_value.returncode=1
                firewall.configure(config,script,apply=True)
            rules=[call.args[0] for call in run.call_args_list if '-I' in call.args[0]]
            self.assertEqual(len(rules),3)
            ssh=next(rule for rule in rules if 'gap-free-vm-ssh' in rule)
            self.assertIn('2121',ssh)
            self.assertNotIn('8092',script.read_text())
            self.assertIn('gap-free-vm-ssh',script.read_text())
            self.assertTrue(all('-F' not in rule for rule in rules))


if __name__=='__main__':unittest.main()
