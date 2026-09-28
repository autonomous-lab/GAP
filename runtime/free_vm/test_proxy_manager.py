import socket
import sys
import unittest

import proxy
sys.modules.setdefault('free_vm_proxy', proxy)
from proxy_manager import ProxyManager


class ProxyManagerTests(unittest.TestCase):
    def test_multiple_vm_listeners_share_one_thread_and_release_ports(self):
        manager=ProxyManager()
        with socket.socket() as probe:
            probe.bind(('127.0.0.1',0))
            first=probe.getsockname()[1]
        with socket.socket() as probe:
            probe.bind(('127.0.0.1',0))
            second=probe.getsockname()[1]
        manager.ensure('vm-a',first)
        manager.ensure('vm-b',second)
        self.assertEqual(len(manager.servers),2)
        self.assertTrue(manager.thread.is_alive())
        manager.release('vm-a')
        manager.release('vm-b')
        self.assertEqual(manager.servers,{})
        with socket.socket() as probe:
            probe.bind(('127.0.0.1',first))
