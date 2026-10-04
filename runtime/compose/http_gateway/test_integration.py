"""Isolated Rust gateway smoke test; no production VM or billing state."""
import concurrent.futures
import contextlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request

PROJECT='prj_'+'a'*24
VM='vm_'+'b'*32
TOKEN='c'*64
EDGE='d'*64


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0))
        return sock.getsockname()[1]


class Control(BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,*args):pass
    def do_POST(self):
        if self.path!='/hot-http' or self.headers.get('Authorization')!='Bearer '+TOKEN:
            self.send_error(403);return
        body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        with self.server.count_lock:
            if body['action']=='begin':
                self.server.begin_count+=1
                ticket=f'{self.server.begin_count:032x}'
            elif body['action']=='end':
                self.server.end_count+=1
        if body['action']=='begin' and self.server.deny_begin:
            data=b'{"error":{"code":"policy_revoked"}}'
            self.send_response(403);self.send_header('Content-Length',str(len(data)))
            self.end_headers();self.wfile.write(data);return
        payload={'port':self.server.backend_port,'ticket':ticket,'cold':False} if body['action']=='begin' else {'ok':True}
        data=json.dumps(payload).encode()
        self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)


class Backend(BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,*args):pass
    def do_GET(self):
        if self.headers.get('Upgrade','').lower()=='websocket':
            self.send_response(101)
            self.send_header('Connection','Upgrade')
            self.send_header('Upgrade','websocket')
            self.end_headers()
            if self.connection.recv(4)==b'PING':self.connection.sendall(b'PONG')
            self.close_connection=True
            return
        data=b'gateway-ok'
        self.server.headers_seen.append(dict(self.headers))
        self.send_response(200);self.send_header('Content-Length',str(len(data)))
        self.end_headers();self.wfile.write(data)


class Integration(unittest.TestCase):
    def test_http_concurrency_and_private_headers(self):
        backend=ThreadingHTTPServer(('127.0.0.1',port()),Backend)
        backend.headers_seen=[]
        control=ThreadingHTTPServer(('127.0.0.1',port()),Control)
        control.backend_port=backend.server_port
        control.begin_count=0;control.end_count=0;control.count_lock=threading.Lock();control.deny_begin=False
        gateway_port=port()
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp,'service.token').write_text(TOKEN)
            Path(tmp,'edge.token').write_text(EDGE)
            image=os.environ.get('GAP_RUST_TEST_IMAGE','gap-compose-fast-snapshot:rust')
            args=['docker','run','--rm','--network','host','--user','0','-v',tmp+':/secrets:ro',
                  '--entrypoint','/usr/local/bin/gap-vm-http-gateway',image,
                  '--bind',f'127.0.0.1:{gateway_port}',
                  '--control',f'http://127.0.0.1:{control.server_port}/hot-http',
                  '--token-file','/secrets/service.token','--edge-token-file','/secrets/edge.token']
            test_binary=os.environ.get('GAP_RUST_TEST_BINARY')
            if test_binary:
                args[args.index('--entrypoint'):args.index('--entrypoint')]=[
                    '-v',test_binary+':/usr/local/bin/gap-vm-http-gateway:ro']
            process=subprocess.Popen(args,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
            try:
                for server in (backend,control):
                    threading.Thread(target=server.serve_forever,daemon=True).start()
                for _ in range(100):
                    try:
                        with socket.create_connection(('127.0.0.1',gateway_port),.1):break
                    except OSError:time.sleep(.05)
                else:self.fail('Rust gateway did not start: '+str(process.poll())+' '+
                               (process.stderr.read(1000).decode() if process.poll() is not None else ''))
                def request(admission=EDGE,vm=VM):
                    headers={'X-GAP-VM-Admission':admission,'X-GAP-Project':PROJECT,'X-GAP-VM':vm}
                    try:
                        with urllib.request.urlopen(urllib.request.Request(
                            f'http://127.0.0.1:{gateway_port}/',headers=headers),timeout=15) as response:
                            return response.status,response.read()
                    except urllib.error.HTTPError as error:
                        with error:return error.code,error.read()
                self.assertEqual(request('bad')[0],403)
                self.assertEqual(request(),(200,b'gateway-ok'))
                with concurrent.futures.ThreadPoolExecutor(max_workers=16) as workers:
                    results=list(workers.map(lambda _:request(),range(100)))
                self.assertTrue(all(result==(200,b'gateway-ok') for result in results))
                self.assertLess(control.begin_count,10,'warm HTTP must not ask Python on each request')
                time.sleep(2.75)
                self.assertEqual(request(),(200,b'gateway-ok'))
                self.assertGreaterEqual(control.begin_count,2,'warm admission must refresh in background')
                control.deny_begin=True
                time.sleep(5.25)
                self.assertEqual(request()[0],403,'revocation must apply within the five-second cache TTL')
                control.deny_begin=False
                self.assertEqual(request(),(200,b'gateway-ok'))
                self.assertTrue(all('X-Gap-Vm-Admission' not in headers and
                                    'X-Gap-Project' not in headers and 'X-Gap-Vm' not in headers
                                    for headers in backend.headers_seen))
                with socket.create_connection(('127.0.0.1',gateway_port),5) as ws:
                    ws.settimeout(5)
                    ws.sendall((f'GET / HTTP/1.1\r\nHost: localhost\r\n'
                        f'Connection: Upgrade\r\nUpgrade: websocket\r\n'
                        f'X-GAP-VM-Admission: {EDGE}\r\nX-GAP-Project: {PROJECT}\r\n'
                        f'X-GAP-VM: {VM}\r\n\r\n').encode())
                    response=b''
                    while b'\r\n\r\n' not in response:response+=ws.recv(4096)
                    self.assertIn(b'101',response.split(b'\r\n',1)[0])
                    ws.sendall(b'PING')
                    self.assertEqual(ws.recv(4),b'PONG')
                with contextlib.ExitStack() as held:
                    for _ in range(64):
                        ws=held.enter_context(socket.create_connection(('127.0.0.1',gateway_port),5))
                        ws.settimeout(5)
                        ws.sendall((f'GET / HTTP/1.1\r\nHost: localhost\r\n'
                            f'Connection: Upgrade\r\nUpgrade: websocket\r\n'
                            f'X-GAP-VM-Admission: {EDGE}\r\nX-GAP-Project: {PROJECT}\r\n'
                            f'X-GAP-VM: {VM}\r\n\r\n').encode())
                        response=b''
                        while b'\r\n\r\n' not in response:response+=ws.recv(4096)
                        self.assertIn(b'101',response.split(b'\r\n',1)[0])
                    self.assertEqual(request()[0],429,'one VM must not occupy all gateway slots')
                    self.assertEqual(request(vm='vm_'+'e'*32),(200,b'gateway-ok'),
                                     'another VM must retain capacity')
            finally:
                process.terminate()
                try:process.communicate(timeout=5)
                except subprocess.TimeoutExpired:process.kill();process.communicate()
                for server in (backend,control):server.shutdown();server.server_close()


if __name__=='__main__':unittest.main()
