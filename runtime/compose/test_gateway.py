import threading
import unittest
from unittest.mock import patch
from lifecycle import Runtime

class SourcePortTests(unittest.TestCase):
    def runtime(self):
        runtime=Runtime.__new__(Runtime);runtime.states={}
        return runtime
    def test_quarantine_survives_hibernation_and_worker_restart(self):
        runtime=self.runtime();meta={'vm_id':'vm_test'}
        with patch('lifecycle.time.monotonic',return_value=100):
            runtime.execution_started(meta,cold=True)
            self.assertTrue(runtime.reserve_tcp_source(meta,40000))
            self.assertFalse(runtime.reserve_tcp_source(meta,40000))
        with patch('lifecycle.time.monotonic',return_value=110):runtime.execution_stopping(meta)
        restored=self.runtime()
        with patch('lifecycle.time.monotonic',return_value=10000):
            restored.execution_started(meta)
            self.assertFalse(restored.reserve_tcp_source(meta,40000))
        with patch('lifecycle.time.monotonic',return_value=10111):
            self.assertTrue(restored.reserve_tcp_source(meta,40000))
    def test_cold_boot_resets_guest_tcp_history(self):
        runtime=self.runtime();meta={'vm_id':'vm_test'}
        runtime.execution_started(meta,cold=True)
        self.assertTrue(runtime.reserve_tcp_source(meta,40000))
        runtime.execution_stopping(meta)
        runtime.execution_started(meta,cold=True)
        self.assertTrue(runtime.reserve_tcp_source(meta,40000))
        self.assertNotIn('tcp_recent_ports',meta)
