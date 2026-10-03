"""Rust proxy control-plane admission and activity accounting."""
import json
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import Mock

from lifecycle import Runtime
from microvm import VMError


PROJECT='prj_'+'a'*24
VM='vm_'+'b'*32


class RustHttpControlTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        catalog=Path(self.temp.name)/'catalog.json'
        self.meta={'project_id':PROJECT,'vm_id':VM,'owner_did':'did:gap:'+'c'*64,
                   'state':'running','ingress':{'enabled':True,'guest_port':8080},
                   'ports':[{'guest_port':8080,'worker_port':58337}]}
        catalog.write_text(json.dumps(self.meta))
        self.runtime=Runtime.__new__(Runtime)
        self.runtime.manager=Mock()
        self.runtime.manager.catalog.return_value=catalog
        self.runtime.manager.read.return_value=self.meta
        self.runtime.ensure_awake=Mock(return_value=self.meta)
        self.runtime.guard=threading.Lock()
        self.runtime.locks={}
        self.runtime.states={}
        self.runtime.http_sessions={}
        self.runtime.http_sessions_guard=threading.Lock()

    def test_begin_touch_end_and_duplicate_end(self):
        result=self.runtime.http_begin(PROJECT,VM)
        self.assertEqual(result['port'],58337)
        self.assertFalse(result['cold'])
        self.assertEqual(self.runtime.state(self.meta)['active_http'],1)
        self.assertTrue(self.runtime.http_touch(result['ticket']))
        self.assertTrue(self.runtime.http_end(result['ticket']))
        self.assertEqual(self.runtime.state(self.meta)['active_http'],0)
        self.assertFalse(self.runtime.http_end(result['ticket']))

    def test_disabled_route_never_wakes_vm(self):
        catalog=self.runtime.manager.catalog.return_value
        catalog.write_text(json.dumps(dict(self.meta,ingress={'enabled':False})))
        with self.assertRaisesRegex(VMError,'unknown_application'):
            self.runtime.http_begin(PROJECT,VM)
        self.runtime.ensure_awake.assert_not_called()

    def test_expired_session_releases_idle_guard(self):
        result=self.runtime.http_begin(PROJECT,VM)
        self.runtime.http_sessions[result['ticket']]=(PROJECT,VM,0)
        self.runtime.prune_http_sessions()
        self.assertEqual(self.runtime.state(self.meta)['active_http'],0)

    def test_warm_credit_check_uses_cache_but_never_expired_lease(self):
        self.runtime.ledger=Mock()
        self.runtime.ledger.view.return_value={'execution_allowed':True,'fleet':{'lease_valid':True}}
        self.runtime.ledger.lease_allowed.return_value=True
        self.runtime.check_credit(self.meta,cached=True)
        self.runtime.check_credit(self.meta,cached=True)
        self.runtime.ledger.view.assert_called_once()
        self.runtime.ledger.sync.assert_not_called()
        self.runtime.ledger.lease_allowed.return_value=False
        with self.assertRaisesRegex(VMError,'fleet_allowance_or_lease_unavailable'):
            self.runtime.check_credit(self.meta,cached=True)


if __name__=='__main__':unittest.main()
