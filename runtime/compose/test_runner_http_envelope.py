"""Service RPC responses must preserve their status and object shape."""
from http.server import ThreadingHTTPServer
import json
import threading
import unittest
import urllib.request

from runner import handler_for


class RunnerEnvelopeTests(unittest.TestCase):
    def test_rpc_status_and_body_are_not_wrapped(self):
        class Fake:
            token='a'*64
            operator_token='b'*64
            def rpc(self,body):return 201,{'policy':'allowed'}
        server=ThreadingHTTPServer(('127.0.0.1',0),handler_for(Fake()))
        thread=threading.Thread(target=server.serve_forever,daemon=True)
        thread.start()
        try:
            request=urllib.request.Request(f'http://127.0.0.1:{server.server_port}/rpc',
                data=b'{}',headers={'Authorization':'Bearer '+'a'*64,'Content-Type':'application/json'})
            with urllib.request.urlopen(request,timeout=3) as response:
                self.assertEqual(response.status,201)
                self.assertEqual(json.load(response),{'policy':'allowed'})
        finally:
            server.shutdown();server.server_close();thread.join(timeout=3)


if __name__=='__main__':unittest.main()
