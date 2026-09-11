import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from costs_env import FIELDS,costs_from_env


class CostEnvTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.path=Path(self.temp.name)/'.env'
        self.values={'GAP_COST_VERSION':'cost-v1',**{k:'' for k in FIELDS}}
        self.values['GAP_COST_NODE_MONTH_USD']='55'
        self.values['GAP_COST_EXTRA_DISK_MONTH_USD']='0'
        self.write()
    def write(self,extra=''):
        self.path.write_text('\n'.join(k+'='+v for k,v in self.values.items())+'\n'+extra)
    def test_blank_is_unknown_zero_is_explicit_and_preview_has_no_secret_access(self):
        self.write('SECRET=do-not-print\nEXEC=$(touch forbidden)')
        costs=costs_from_env(self.path)
        self.assertEqual(costs['node_month_microdollars'],55_000_000)
        self.assertEqual(costs['extra_disk_month_microdollars'],0)
        self.assertIsNone(costs['network_in_gb_microdollars'])
        result=subprocess.run([sys.executable,str(Path(__file__).with_name('microvm-billing.py')),'--token-file','missing','preview-costs-env','--env-file',str(self.path)],capture_output=True,text=True,check=True)
        self.assertEqual(json.loads(result.stdout),costs)
        self.assertNotIn('do-not-print',result.stdout+result.stderr)
    def test_cost_parser_rejects_ambiguous_or_missing_fields(self):
        for value in ['NaN','-1','1e3','$(touch forbidden)','0.0000001']:
            self.values['GAP_COST_NODE_MONTH_USD']=value;self.write()
            with self.assertRaises(ValueError):costs_from_env(self.path)
        self.values['GAP_COST_NODE_MONTH_USD']='55';self.write('GAP_COST_NODE_MONTH_USD=56')
        with self.assertRaises(ValueError):costs_from_env(self.path)
        del self.values['GAP_COST_NODE_MONTH_USD'];self.write()
        with self.assertRaises(ValueError):costs_from_env(self.path)
    def test_authenticated_apply_is_guarded_and_does_not_change_tariff(self):
        import threading
        from types import SimpleNamespace
        from http.server import ThreadingHTTPServer
        sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'runtime/compose'))
        from billing import Ledger
        from runner import Runner,handler_for
        root=Path(self.temp.name);runner=Runner.__new__(Runner)
        runner.token='s'*64;runner.operator_token='o'*64
        runner.runtime=SimpleNamespace(ledger=Ledger(root/'ledger.sqlite'))
        token=root/'operator.token';token.write_text(runner.operator_token)
        server=ThreadingHTTPServer(('127.0.0.1',0),handler_for(runner))
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        try:
            command=[sys.executable,str(Path(__file__).with_name('microvm-billing.py')),'--token-file',str(token),'--endpoint','http://127.0.0.1:'+str(server.server_port)+'/operator','set-costs-env','--env-file',str(self.path),'--expect-version','none']
            first=subprocess.run(command,capture_output=True,text=True,check=True)
            self.assertEqual(json.loads(first.stdout)['version'],'cost-v1')
            stale=subprocess.run(command,capture_output=True,text=True)
            self.assertNotEqual(stale.returncode,0)
            self.assertIn('cost_version_changed',stale.stderr)
            self.assertIsNone(runner.runtime.ledger.pricing()['tariff'])
            self.assertNotIn(runner.operator_token,first.stdout+stale.stderr)
        finally:server.shutdown();server.server_close();thread.join()


if __name__=='__main__':unittest.main()
