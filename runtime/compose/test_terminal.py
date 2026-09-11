import base64
import sys
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch
from terminal import Session, Terminals, TerminalError


class TerminalTests(unittest.TestCase):
    def session(self, command=None):
        # Test process only. Production command is fixed to controller SSH.
        s=Session(command or [sys.executable,'-u','-c','import sys; print("READY"); [print("RX:"+x.strip()) for x in sys.stdin]'],('p','o','v'),80,24)
        self.addCleanup(s.close)
        time.sleep(.1)
        return s

    def test_input_retry_and_output_replay(self):
        s=self.session();body={'seq':1,'cursor':0,'input':base64.b64encode(b'hello\n').decode(),'cols':90,'rows':30}
        s.exchange(body);time.sleep(.1)
        a=s.exchange(body);b=s.exchange(body)
        self.assertEqual(a['output'],b['output'])
        self.assertEqual(base64.b64decode(b['output']).count(b'RX:hello'),1)
        with self.assertRaises(TerminalError):s.exchange(dict(body,input='eA=='))
        with self.assertRaises(TerminalError):s.exchange(dict(body,seq=3))
        with self.assertRaises(TerminalError):s.exchange(dict(body,cursor=999999))
        self.assertEqual(s.process.poll(),None)
        s.close();self.assertIsNotNone(s.process.poll())

    def test_output_bounded_and_safe_cursor(self):
        s=self.session([sys.executable,'-u','-c','import time; print("X"*2000000); time.sleep(10)'])
        deadline=time.time()+3
        while s.start==0 and time.time()<deadline:time.sleep(.05)
        self.assertGreater(s.start,0)
        self.assertLessEqual(len(s.buffer),1024*1024)
        result=s.exchange({'seq':1,'cursor':0,'input':'','cols':80,'rows':24})
        self.assertTrue(result['truncated']);self.assertLessEqual(len(base64.b64decode(result['output'])),65536)

    def test_dimensions_and_scope(self):
        s=self.session()
        with self.assertRaises(TerminalError):s.resize(0,100)
        with self.assertRaises(TerminalError):s.resize(True,100)
        with patch('terminal.threading.Thread.start'):
            manager=Terminals(SimpleNamespace())
        manager.sessions[s.id]=s
        for identity in [('p','other','v'),('other','o','v'),('p','o','other')]:
            with self.assertRaises(TerminalError):manager.rpc(*identity[:2],'close',{'vm_id':identity[2],'terminal_id':s.id})
        self.assertFalse(s.closed)
        manager.close_vm('v');self.assertTrue(s.closed)

    def test_closed_terminal_drains_output(self):
        s=self.session([sys.executable,'-u','-c','print("finished")'])
        deadline=time.time()+2
        while not s.closed and time.time()<deadline:time.sleep(.02)
        r=s.exchange({'seq':1,'cursor':0,'input':'','cols':80,'rows':24})
        self.assertTrue(r['closed']);self.assertIn(b'finished',base64.b64decode(r['output']))

    def test_terminal_key_preserves_forced_deployment_command(self):
        import tempfile
        from pathlib import Path
        from microvm import MicroVMs
        with tempfile.TemporaryDirectory() as d:
            folder=Path(d);(folder/'client_key.pub').write_text('ssh-ed25519 DEPLOY')
            (folder/'terminal_key.pub').write_text('ssh-ed25519 TERMINAL')
            m=MicroVMs.__new__(MicroVMs);m.folder=lambda meta:folder
            keys=m.authorized_keys({},['ssh-rsa USER'])
            self.assertIn('restrict,command="python3 /usr/local/lib/gap-compose-guest.py" ssh-ed25519 DEPLOY',keys)
            self.assertIn('restrict,pty,command="/bin/sh -l" ssh-ed25519 TERMINAL',keys)
            self.assertIn('no-agent-forwarding,no-X11-forwarding ssh-rsa USER',keys)

    def test_polling_does_not_touch_activity_but_input_does(self):
        s=self.session()
        meta={'state':'running'}; touches=[]
        runtime=SimpleNamespace(check_policy=lambda m:True,check_credit=lambda m:None,touch=lambda m:touches.append(m))
        runner=SimpleNamespace(hypervisor=SimpleNamespace(read=lambda *args:meta),runtime=runtime)
        with patch('terminal.threading.Thread.start'):manager=Terminals(runner)
        manager.sessions[s.id]=s
        body={'vm_id':'v','terminal_id':s.id,'seq':1,'cursor':0,'input':'','cols':80,'rows':24}
        manager.rpc('p','o','io',body);self.assertEqual(touches,[])
        body.update(seq=2,input='eA==');manager.rpc('p','o','io',body);self.assertEqual(len(touches),1)
        manager.rpc('p','o','io',body);self.assertEqual(len(touches),1)
        runtime.check_policy=lambda m:False
        with self.assertRaises(TerminalError):manager.rpc('p','o','io',body)
        self.assertTrue(s.closed)
