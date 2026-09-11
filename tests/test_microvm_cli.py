import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('microvm_cli', Path(__file__).resolve().parents[1]/'scripts/microvm.py')
cli=importlib.util.module_from_spec(spec);spec.loader.exec_module(cli)
P='prj_'+'a'*24
V='vm_'+'b'*32

class CLITests(unittest.TestCase):
    def request(self, *args):
        captured=[]
        def open_request(request, **kwargs):
            captured.append(request)
            return io.BytesIO(b'{}')
        with patch.object(sys,'argv',['microvm.py','--project',P,'--vm',V,*args]), patch.dict('os.environ',{'GAP_TOKEN':'test-bearer'}), patch.object(cli.urllib.request,'build_opener') as opener, contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            opener.return_value.open.side_effect=open_request
            self.assertEqual(cli.main(),0)
        request=captured[0]
        return request.method,request.full_url,json.loads(request.data) if request.data else None

    def test_native_lifecycle_routes_and_generation(self):
        method,url,body=self.request('create','--stopped','--guest-port','8000')
        self.assertEqual((method,url),('POST','https://gap.geta.team/v1/cloud/projects/'+P+'/vm'))
        self.assertNotIn('vm_id',body)
        self.assertFalse(body['start'])
        self.assertEqual(body['ports'],[8000])
        for action, method, suffix in [('show','GET',''),('start','POST','/start'),('stop','POST','/stop'),('resize','PATCH',''),('destroy','DELETE','')]:
            m,url,body=self.request(action)
            self.assertEqual(m,method)
            self.assertTrue(url.endswith('/vm'+suffix))
            if body:
                self.assertEqual(body['vm_id'],V)
        _,_,body=self.request('destroy')
        self.assertFalse(body['delete_data'])
        with self.assertRaises(SystemExit):
            self.request('destroy','--delete-data')

    def test_network_routes_and_invalid_ingress(self):
        _,url,body=self.request('set-ingress','--guest-port','8000')
        self.assertTrue(url.endswith('/vm/ingress'));self.assertEqual(body['guest_port'],8000)
        _,_,body=self.request('set-ingress','--disable')
        self.assertEqual(body['enabled'],False);self.assertNotIn('guest_port',body)
        with self.assertRaises(SystemExit):
            self.request('set-ingress','--disable','--guest-port','8000')
        _,url,body=self.request('set-ports','--map','1:22:tcp')
        self.assertTrue(url.endswith('/vm/ports'));self.assertEqual(body['mappings'],[{'slot':1,'guest_port':22,'protocol':'tcp'}])
        _,url,_=self.request('job','job_'+'c'*32)
        self.assertIn('/vm/jobs/job_',url)

if __name__=='__main__': unittest.main()
