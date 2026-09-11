import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from lifecycle import Runtime
from microvm import MicroVMs,VMError
import threading


class SuspensionTests(unittest.TestCase):
    def test_preemption_does_not_wait_indefinitely_for_a_busy_monitor(self):
        manager=MicroVMs.__new__(MicroVMs);manager.qmp_locks_guard=threading.Lock()
        lock=threading.Lock();lock.acquire();manager.qmp_locks={'test-vm':lock}
        try:
            with self.assertRaisesRegex(VMError,'qmp_monitor_busy'):
                manager.qmp({'vm_id':'test-vm'},'stop')
        finally:lock.release()
    def runtime(self):
        runtime=Runtime.__new__(Runtime);runtime.states={};runtime.closed=False
        meta=dict(vm_id='vm_'+'a'*32,project_id='prj_'+'b'*24,owner_did='did:gap:'+'c'*64,state='running')
        return runtime,meta
    def test_policy_lease_expiry_denies_stale_generation_and_missing_authority(self):
        runtime,meta=self.runtime()
        policy={'owner_did':meta['owner_did'],'generation':1,'allowed':True,'suspended':False}
        runtime.runner=SimpleNamespace(workload_policy=lambda p,o:policy)
        with patch('lifecycle.time.monotonic',return_value=10):self.assertTrue(runtime.check_policy(meta))
        policy.update(generation=2,allowed=False,suspended=True)
        with patch('lifecycle.time.monotonic',return_value=14):self.assertTrue(runtime.check_policy(meta))
        with patch('lifecycle.time.monotonic',return_value=16):self.assertFalse(runtime.check_policy(meta))
        policy.update(generation=1,allowed=True,suspended=False)
        self.assertFalse(runtime.check_policy(meta,force=True))
        policy.update(generation=3)
        self.assertTrue(runtime.check_policy(meta,force=True))
        policy.clear()
        self.assertFalse(runtime.check_policy(meta,force=True))
        self.assertEqual(runtime.state(meta)['policy_generation'],3)
    def test_watchdog_closes_connections_and_stops_cpu_without_job_lock(self):
        runtime,meta=self.runtime();calls=[]
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'catalog').mkdir();path=root/'catalog/vm.json';path.write_text(json.dumps(meta))
            runtime.manager=SimpleNamespace(root=root,alive=lambda m:True,qmp=lambda m,c:calls.append(c),children={})
            runtime.runner=SimpleNamespace(workload_policies=lambda p:{meta['project_id']:{'owner_did':meta['owner_did'],'generation':1,'allowed':False,'suspended':True}})
            sock=SimpleNamespace(shutdown=lambda n:calls.append('shutdown'),close=lambda:calls.append('close'))
            # A hashable fake socket, matching the runtime's tracked connection set.
            class Socket:
                def shutdown(self,n):sock.shutdown(n)
                def close(self):sock.close()
            runtime.state(meta)['connections'].add(Socket())
            def end_loop(_):runtime.closed=True
            with patch('lifecycle.time.sleep',side_effect=end_loop):runtime.policy_watchdog()
            self.assertEqual(calls,['shutdown','close','stop'])
            self.assertFalse(runtime.state(meta)['policy_allowed'])
            self.assertIsNone(runtime.state(meta)['running_since'])
            self.assertEqual(json.loads(path.read_text()),meta)
            # An in-progress snapshot already has its own stop operation; don't
            # interrupt it through a second competing QMP connection.
            runtime.closed=False;calls.clear();meta['state']='hibernating';path.write_text(json.dumps(meta))
            with patch('lifecycle.time.sleep',side_effect=end_loop):runtime.policy_watchdog()
            self.assertEqual(calls,[])


if __name__=='__main__':unittest.main()
