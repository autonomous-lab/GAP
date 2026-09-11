import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from metrics import Metrics


class MetricsTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.folder=Path(self.tmp.name)
        self.manager=SimpleNamespace(network=None,meters={},folder=lambda m:self.folder)
        self.runtime=SimpleNamespace(storage_bytes=lambda m:2000)
        self.metrics=Metrics(SimpleNamespace(hypervisor=self.manager,runtime=self.runtime))
        self.meta={'vm_id':'vm_'+'a'*32,'state':'running','vcpus':.25,'memory_mib':256,'disk_gib':4}
        self.process={'identity':(1,1),'cpu_seconds':1,'resident_bytes':1000}
        self.metrics.process=lambda meta:dict(self.process)
        self.now=10
        p=patch('metrics.time.monotonic',side_effect=lambda:self.now);p.start();self.addCleanup(p.stop)

    def counters(self,a,b):
        (self.folder/'network-counts.json').write_text(json.dumps({'in':a,'out':b}))

    def test_fractional_cpu_and_network_rates_without_guest_activity(self):
        self.counters(100,200);first=self.metrics.read(self.meta)
        self.assertIsNone(first['cpu_percent_of_allocation'])
        self.process['cpu_seconds']=1.5;self.now+=2;self.counters(300,800)
        result=self.metrics.read(self.meta)
        self.assertEqual(result['cpu_percent_of_allocation'],100)
        self.assertEqual(result['network_in_bytes_per_second'],100)
        self.assertEqual(result['network_out_bytes_per_second'],300)
        self.assertEqual(result['memory_resident_bytes'],1000)
        self.assertEqual(result['storage_host_bytes'],2000)
        self.assertEqual(result['disk_capacity_bytes'],4*1024**3)
        self.assertEqual(self.meta['state'],'running')
        # No touch(), ensure_awake(), SSH or QMP exists in this fixture.

    def test_new_process_and_counter_reset_do_not_create_spikes(self):
        self.counters(500,500);self.metrics.read(self.meta)
        self.now+=10;self.process.update(identity=(2,2),cpu_seconds=.2);self.counters(1,1)
        result=self.metrics.read(self.meta)
        self.assertIsNone(result['cpu_percent_of_allocation'])
        self.assertIsNone(result['network_in_bytes_per_second'])
        self.process.update(identity=None,cpu_seconds=0,resident_bytes=0);self.meta['state']='hibernated';self.now+=10
        result=self.metrics.read(self.meta)
        self.assertEqual(result['cpu_percent_of_allocation'],0)
        self.assertEqual(result['memory_resident_bytes'],0)
        self.assertEqual(result['allocated_memory_bytes'],256*1024**2)

    def test_unavailable_is_not_reported_as_zero_and_absent_has_no_metrics(self):
        self.assertEqual(self.metrics.read(None),{'available':False})
        self.assertEqual(self.metrics.read(dict(self.meta,state='destroyed')),{'available':False})
        (self.folder/'network-counts.json').write_text('{broken')
        self.metrics.process=lambda meta:(_ for _ in ()).throw(OSError())
        result=self.metrics.read(self.meta)
        self.assertIsNone(result['cpu_percent_of_allocation']);self.assertIsNone(result['network_in_bytes'])
        self.assertEqual(set(result['errors']),{'process_metrics_unavailable','network_metrics_unavailable'})

    def test_fast_refresh_keeps_original_baseline(self):
        self.metrics.read(self.meta);self.now+=.2;self.metrics.read(self.meta)
        self.now+=1.8;self.process['cpu_seconds']=1.5
        self.assertEqual(self.metrics.read(self.meta)['cpu_percent_of_allocation'],100)

    def test_linux_proc_fields_use_process_cpu_and_resident_pages(self):
        import os
        (self.folder/'qemu.pid').write_text(str(os.getpid()))
        self.manager.alive=lambda meta:True
        actual=Metrics(SimpleNamespace(hypervisor=self.manager,runtime=self.runtime)).process(self.meta)
        self.assertEqual(actual['identity'][0],os.getpid())
        self.assertGreater(actual['identity'][1],0)
        self.assertGreater(actual['resident_bytes'],0)
        self.assertGreaterEqual(actual['cpu_seconds'],0)
