from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from egress_policy import chains, remove_output_references


VM = 'vm_' + 'a' * 32
OLD = '/system.slice/docker-' + 'b' * 64 + '.scope/' + VM


class EgressPolicyTests(unittest.TestCase):
    def test_stale_worker_rule_is_removed_for_the_exact_vm(self):
        name = chains(VM)[0]
        listing = f'-A OUTPUT -m cgroup --path "{OLD}" -j {name}\n'
        with (patch('egress_policy.run', return_value=SimpleNamespace(returncode=0, stdout=listing)),
              patch('egress_policy.Path.exists', return_value=False),
              patch('egress_policy.required') as required):
            remove_output_references(123, 4, name, VM)
        required.assert_called_once_with(123, 4, '-D', 'OUTPUT', '-m', 'cgroup', '--path', OLD, '-j', name)

    def test_live_old_cgroup_is_not_removed(self):
        name = chains(VM)[0]
        listing = f'-A OUTPUT -m cgroup --path "{OLD}" -j {name}\n'
        with (patch('egress_policy.run', return_value=SimpleNamespace(returncode=0, stdout=listing)),
              patch('egress_policy.Path.exists', return_value=True),
              patch('egress_policy.Path.read_text', return_value='1234')):
            with self.assertRaisesRegex(RuntimeError, 'vm_still_running'):
                remove_output_references(123, 4, name, VM)

    def test_unexpected_reference_fails_closed(self):
        name = chains(VM)[0]
        listing = f'-A OUTPUT -j {name}\n'
        with patch('egress_policy.run', return_value=SimpleNamespace(returncode=0, stdout=listing)):
            with self.assertRaisesRegex(RuntimeError, 'unexpected_egress_firewall_rule'):
                remove_output_references(123, 4, name, VM)


if __name__ == '__main__':
    unittest.main()
