import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from pricing_env import FIELDS, tariff_from_env


class PricingEnvTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.path=Path(self.temp.name)/'.env'
        self.values={'GAP_PRICING_VERSION':'usd-v1',**{k:'0.010' for k in FIELDS}}
        self.values['GAP_PRICE_DISK_GB_MONTH_USD']='0.10'

    def write(self,extra=''):
        self.path.write_text('\n'.join(k+'='+v for k,v in self.values.items())+'\n'+extra)

    def test_exact_units_and_secret_free_offline_preview(self):
        self.write('UNRELATED_SECRET=never-show-this\nUNRELATED_COMMAND=$(touch forbidden)')
        tariff=tariff_from_env(self.path)
        self.assertEqual(tariff,{'version':'usd-v1','vcpu_hour':10000,'gib_ram_hour':10000,'gb_disk_month':100000,'gb_in':10000,'gb_out':10000})
        result=subprocess.run([sys.executable,str(Path(__file__).with_name('microvm-billing.py')),
            '--token-file','does-not-exist','preview-pricing-env','--env-file',str(self.path)],capture_output=True,text=True,check=True)
        self.assertEqual(json.loads(result.stdout),tariff)
        self.assertNotIn('never-show-this',result.stdout+result.stderr)

    def test_quotes_export_comments_and_microcredit_precision(self):
        self.values['GAP_PRICE_VCPU_HOUR_USD']='"0.000001" # precision'
        self.values['GAP_PRICE_RAM_GIB_HOUR_USD']="'0.01'"
        self.write();self.path.write_text('export '+self.path.read_text())
        self.assertEqual(tariff_from_env(self.path)['vcpu_hour'],1)

    def test_rejects_missing_duplicate_executable_and_ambiguous_amounts(self):
        for value in ['NaN','Infinity','-0.1','1e-2','0.0000001','$(touch forbidden)','1000001', '"0.1"junk']:
            with self.subTest(value=value):
                self.values['GAP_PRICE_VCPU_HOUR_USD']=value;self.write()
                with self.assertRaises(ValueError):tariff_from_env(self.path)
        self.values['GAP_PRICE_VCPU_HOUR_USD']='0.01'
        self.write('GAP_PRICE_VCPU_HOUR_USD=0.02')
        with self.assertRaisesRegex(ValueError,'duplicate'):tariff_from_env(self.path)
        del self.values['GAP_PRICE_VCPU_HOUR_USD'];self.write()
        with self.assertRaisesRegex(ValueError,'missing'):tariff_from_env(self.path)

    def test_no_implicit_free_production_tariff(self):
        self.values.update({k:'0' for k in FIELDS});self.write()
        with self.assertRaisesRegex(ValueError,'zero'):tariff_from_env(self.path)

    def test_cli_applies_through_authenticated_operator_with_version_guard(self):
        import threading
        from types import SimpleNamespace
        from http.server import ThreadingHTTPServer
        sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'runtime/compose'))
        from billing import Ledger
        from runner import Runner, handler_for
        root=Path(self.temp.name);(root/'catalog').mkdir()
        runner=Runner.__new__(Runner)
        runner.token='s'*64;runner.operator_token='o'*64
        runner.hypervisor=SimpleNamespace(root=root)
        runner.runtime=SimpleNamespace(ledger=Ledger(root/'ledger.sqlite'),lock=lambda _:threading.RLock())
        token=root/'operator.token';token.write_text(runner.operator_token)
        self.write()
        server=ThreadingHTTPServer(('127.0.0.1',0),handler_for(runner))
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        try:
            command=[sys.executable,str(Path(__file__).with_name('microvm-billing.py')),
                     '--token-file',str(token),'--endpoint','http://127.0.0.1:'+str(server.server_port)+'/operator',
                     'set-pricing-env','--env-file',str(self.path),'--expect-version','none']
            first=subprocess.run(command,capture_output=True,text=True,check=True)
            self.assertEqual(json.loads(first.stdout)['tariff']['version'],'usd-v1')
            self.values['GAP_PRICING_VERSION']='usd-v2';self.write()
            stale=subprocess.run(command,capture_output=True,text=True)
            self.assertNotEqual(stale.returncode,0)
            self.assertIn('tariff_version_changed',stale.stderr)
            self.assertEqual(runner.runtime.ledger.pricing()['tariff']['version'],'usd-v1')
            command[-1]='usd-v1'
            updated=subprocess.run(command,capture_output=True,text=True,check=True)
            self.assertEqual(json.loads(updated.stdout)['tariff']['version'],'usd-v2')
            self.assertNotIn(runner.operator_token,first.stdout+stale.stderr+updated.stdout)
        finally:
            server.shutdown();server.server_close();thread.join()


if __name__=='__main__':unittest.main()
