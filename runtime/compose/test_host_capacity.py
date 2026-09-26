import unittest
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import threading
from types import SimpleNamespace
from unittest.mock import patch

from host_capacity import commitment_memory, cpu_limit, startable_memory, vm_limit
from lifecycle import Runtime
from microvm import VMError


class HostCapacity(unittest.TestCase):
    def test_eightfold_cpu_and_vm_limits_scale_with_host_cores(self):
        self.assertEqual(cpu_limit(8, 1, 8), 56)
        self.assertEqual(cpu_limit(8, 0, 8), 64)
        self.assertEqual(vm_limit(8, 8), 64)
        self.assertEqual(cpu_limit(16, 0, 8), 128)
        self.assertEqual(vm_limit(16, 8), 128)
        self.assertEqual(cpu_limit(1, 1, 8), 1)

    def test_swap_adds_capacity_but_physical_and_swap_reserves_remain(self):
        memory = dict(MemTotal=16384, MemAvailable=6144, SwapTotal=16384, SwapFree=12288)
        self.assertEqual(startable_memory(memory, 2048, 2048), 14336)
        self.assertEqual(commitment_memory(memory, 8192, 2048, 2048), 20480)
        self.assertEqual(startable_memory(dict(memory, MemAvailable=256), 2048, 2048), 0)
        self.assertEqual(startable_memory(dict(memory, MemAvailable=1024), 2048, 2048), 9216)
        self.assertEqual(startable_memory(dict(memory, SwapFree=1024), 2048, 2048), 4096)
        self.assertEqual(commitment_memory(memory, 8192, 2048, 0), 22528)
        self.assertEqual(startable_memory(memory, 2048, 0), 16384)

    def test_admission_counts_hibernated_vm_slots_but_allows_their_wake(self):
        with TemporaryDirectory() as tmp:
            catalog = Path(tmp) / 'catalog'
            catalog.mkdir()
            for number in range(64):
                (catalog / f'{number}.json').write_text(json.dumps(dict(vm_id=f'vm_{number:032x}',state='hibernated',vcpus=1)))
            runtime = SimpleNamespace(manager=SimpleNamespace(root=Path(tmp),alive=lambda meta: False),
                                      capacity_lock=threading.RLock(),reserve_memory_mib=2048,
                                      reserve_swap_mib=0,min_available_memory_mib=512,
                                      reserve_vcpus=0,cpu_overcommit_ratio=8,vms_per_cpu=8)
            memory = dict(MemTotal=16384,MemAvailable=14000,SwapTotal=16384,SwapFree=16384)
            with patch('host_capacity.logical_cpus',return_value=8),patch('host_capacity.memory_mib',return_value=memory):
                with self.assertRaisesRegex(VMError,'host_capacity_unavailable'):
                    with Runtime.admission(runtime,'project',1,256):
                        pass
                with Runtime.admission(runtime,'project',1,256,'vm_' + f'{0:032x}'):
                    pass


if __name__ == '__main__':
    unittest.main()
