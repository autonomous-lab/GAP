import unittest

from host_capacity import commitment_memory, cpu_limit, startable_memory


class HostCapacity(unittest.TestCase):
    def test_eightfold_cpu_overcommit_reserves_one_host_core(self):
        self.assertEqual(cpu_limit(8, 1, 8), 56)
        self.assertEqual(cpu_limit(1, 1, 8), 1)

    def test_swap_adds_capacity_but_physical_and_swap_reserves_remain(self):
        memory = dict(MemTotal=16384, MemAvailable=6144, SwapTotal=16384, SwapFree=12288)
        self.assertEqual(startable_memory(memory, 2048, 2048), 14336)
        self.assertEqual(commitment_memory(memory, 8192, 2048, 2048), 20480)
        self.assertEqual(startable_memory(dict(memory, MemAvailable=256), 2048, 2048), 0)
        self.assertEqual(startable_memory(dict(memory, MemAvailable=1024), 2048, 2048), 9216)
        self.assertEqual(startable_memory(dict(memory, SwapFree=1024), 2048, 2048), 4096)


if __name__ == '__main__':
    unittest.main()
