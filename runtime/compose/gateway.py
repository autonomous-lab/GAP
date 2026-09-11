"""Always-present inbound HTTP/WS/TCP/UDP wake gateway, separate from QEMU.

Outbound traffic is metered by the QEMU filter, never an idle-timer signal.
"""
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import re
import select
import socket
import socketserver
import threading
import time

from microvm import VMError, free_port

HOP={'connection','keep-alive','proxy-authenticate','proxy-authorization','te','trailer','transfer-encoding','upgrade'}
MAX_BODY=16*1024*1024


class LimitedThreads:
    def __init__(self,*args,**kwargs):
        self.slots=threading.BoundedSemaphore(128)
        super().__init__(*args,**kwargs)
    def process_request(self,request,address):
        if not self.slots.acquire(blocking=False):
            self.shutdown_request(request); return
        try: super().process_request(request,address)
        except BaseException:
            self.slots.release(); raise
    def process_request_thread(self,request,address):
        try: super().process_request_thread(request,address)
        finally: self.slots.release()


class HTTPServer(LimitedThreads,ThreadingHTTPServer):
    daemon_threads=True


class TCPServer(LimitedThreads,socketserver.ThreadingTCPServer):
    allow_reuse_address=True
    daemon_threads=True


class UDPServer(LimitedThreads,socketserver.ThreadingUDPServer):
    allow_reuse_address=True
    daemon_threads=True


class Gateway:
    def __init__(self,runtime,port=8094):
        self.runtime=runtime; runtime.gateway=self
        self.manager=runtime.manager
        self.port=port; self.listeners={}; self.guard=threading.RLock()
        self.udp_peers={}
        outer=self
        class HTTP(BaseHTTPRequestHandler):
            protocol_version='HTTP/1.1'
            def log_message(self,*args): pass
            def do_GET(self): outer.http(self)
            do_POST=do_PUT=do_PATCH=do_DELETE=do_HEAD=do_OPTIONS=do_GET
        self.http_server=HTTPServer(('127.0.0.1',port),HTTP)
        self.http_server.daemon_threads=True
        threading.Thread(target=self.http_server.serve_forever,daemon=True).start()

    def destination(self,project,slot,protocol):
        with self.runtime.lock(project):
            meta=self.runtime.ensure_awake(project)
            mapping=next((m for m in meta.get('public_mappings',[]) if m['slot']==slot and m['protocol'] in (protocol,'both')),None)
            if not mapping: raise VMError('public_mapping_unavailable')
            return meta,meta['public_targets'][str(slot)]

    def connect(self,port,meta):
        deadline=time.monotonic()+30
        with self.runtime.lock(meta['project_id']):
            current=self.manager.read(meta['project_id'],meta['owner_did'])
            if not current or current['vm_id']!=meta['vm_id'] or current['state']!='running':
                raise VMError('application_changed_retry_request')
            while time.monotonic()<deadline:
                with self.manager.allocation_lock():
                    reserved=self.manager.reserved_ports()
                    for _ in range(1000):
                        connection=socket.socket(socket.AF_INET,socket.SOCK_STREAM)
                        connection.settimeout(2)
                        connection.bind(('127.0.0.1',0))
                        source=connection.getsockname()[1]
                        if source not in reserved and self.runtime.reserve_tcp_source(meta,source): break
                        connection.close()
                    else:raise VMError('tcp_source_ports_busy_retry_later')
                    try:
                        connection.connect(('127.0.0.1',port))
                        return connection
                    except OSError:connection.close()
                time.sleep(.025)
        raise VMError('application_not_ready')

    def tunnel(self,meta,client,upstream):
        state=self.runtime.state(meta)
        with self.runtime.lock(meta['project_id']): state['connections'].update((client,upstream))
        try:
            client.settimeout(10); upstream.settimeout(10)
            while True:
                readers,_,_=select.select([client,upstream],[],[],1)
                for source in readers:
                    data=source.recv(65536)
                    if not data: return
                    if source is client:
                        with self.runtime.lock(meta['project_id']): self.runtime.touch(meta)
                    (upstream if source is client else client).sendall(data)
        except (OSError,ValueError): pass
        finally:
            with self.runtime.lock(meta['project_id']):
                state['connections'].discard(client); state['connections'].discard(upstream)
            upstream.close()

    def tcp_handler(self,project,slot):
        outer=self
        class Handler(socketserver.BaseRequestHandler):
            def handle(self):
                try:
                    meta,port=outer.destination(project,slot,'tcp')
                    outer.tunnel(meta,self.request,outer.connect(port,meta))
                except (VMError,OSError): pass
        return Handler

    def udp_handler(self,project,slot):
        outer=self
        class Handler(socketserver.BaseRequestHandler):
            def handle(self):
                data,server=self.request
                if len(data)>65507: return
                try:
                    meta,port=outer.destination(project,slot,'udp')
                    identity=(project,slot,self.client_address)
                    with outer.guard:
                        peer=outer.udp_peers.get(identity)
                        if peer is None:
                            if len(outer.udp_peers)>=256: return
                            upstream=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)
                            upstream.connect(('127.0.0.1',port)); upstream.settimeout(1)
                            peer={'socket':upstream,'last':time.monotonic()}
                            outer.udp_peers[identity]=peer
                            def receive():
                                try:
                                    while time.monotonic()-peer['last']<60:
                                        try: packet=upstream.recv(65535)
                                        except socket.timeout: continue
                                        server.sendto(packet,self.client_address)
                                except OSError: pass
                                finally:
                                    upstream.close()
                                    with outer.guard: outer.udp_peers.pop(identity,None)
                            threading.Thread(target=receive,daemon=True).start()
                        peer['last']=time.monotonic(); peer['socket'].send(data)
                except (VMError,OSError): pass
        return Handler

    def withdraw(self,project):
        with self.guard:
            for identity,server in list(self.listeners.items()):
                if identity[0]==project:
                    server.shutdown(); server.server_close(); del self.listeners[identity]
            for identity,peer in list(self.udp_peers.items()):
                if identity[0]==project:
                    peer['socket'].close(); del self.udp_peers[identity]

    def reconcile(self):
        wanted={}
        for path in (self.manager.root/'catalog').glob('prj_*.json'):
            meta=json.loads(path.read_text())
            if meta['state'] not in ('running','hibernated','hibernating','resuming'): continue
            for mapping in meta.get('public_mappings',[]):
                for protocol in ('tcp','udp'):
                    if mapping['protocol'] in (protocol,'both'):
                        identity=(meta['project_id'],mapping['slot'],protocol,meta['public_ports'][mapping['slot']-1])
                        wanted[identity]=mapping
        with self.guard:
            for identity,server in list(self.listeners.items()):
                if identity not in wanted:
                    server.shutdown(); server.server_close(); del self.listeners[identity]
            for identity in wanted:
                if identity in self.listeners: continue
                project,slot,protocol,port=identity
                cls=TCPServer if protocol=='tcp' else UDPServer
                handler=self.tcp_handler(project,slot) if protocol=='tcp' else self.udp_handler(project,slot)
                server=cls(('0.0.0.0',port),handler)
                self.listeners[identity]=server
                threading.Thread(target=server.serve_forever,daemon=True).start()

    def request_body(self,handler):
        if handler.headers.get('Transfer-Encoding','').lower()=='chunked':
            if handler.headers.get('Content-Length'): raise VMError('ambiguous_body')
            body=bytearray()
            while True:
                line=handler.rfile.readline(128)
                size=int(line.split(b';')[0].strip(),16)
                if size<0 or len(body)+size>MAX_BODY: raise VMError('request_body_too_large')
                if not size:
                    for _ in range(100):
                        if handler.rfile.readline(8192)==b'\r\n': return bytes(body)
                    raise VMError('invalid_chunk_trailers')
                chunk=handler.rfile.read(size)
                if len(chunk)!=size or handler.rfile.read(2)!=b'\r\n': raise VMError('invalid_chunk')
                body.extend(chunk)
        if handler.headers.get('Transfer-Encoding'): raise VMError('unsupported_transfer_encoding')
        lengths=handler.headers.get_all('Content-Length',[])
        if len(lengths)>1: raise VMError('ambiguous_body')
        size=int(lengths[0]) if lengths else 0
        if not 0<=size<=MAX_BODY: raise VMError('request_body_too_large')
        body=handler.rfile.read(size)
        if len(body)!=size: raise VMError('incomplete_body')
        return body

    def http(self,h):
        sent=False; upstream=None
        try:
            h.connection.settimeout(120)
            project=h.headers.get('X-GAP-Project','')
            if not re.fullmatch(r'prj_[0-9a-f]{24}',project): raise VMError('unknown_application')
            path=self.manager.catalog(project)
            if not path.exists(): raise VMError('unknown_application')
            initial=json.loads(path.read_text())
            if not initial.get('ingress',{}).get('enabled'): raise VMError('unknown_application')
            upgrade=h.headers.get('Upgrade','').lower()=='websocket'
            with self.runtime.http_request(project) as meta:
                with self.runtime.lock(project):
                    current=self.manager.read(project,meta['owner_did'])
                    if not current or current['vm_id']!=meta['vm_id'] or current['state']!='running' or not current.get('ingress',{}).get('enabled'):
                        raise VMError('application_changed_retry_request')
                    port=next(p['worker_port'] for p in current['ports'] if p['guest_port']==current['ingress']['guest_port'])
                    upstream=self.connect(port,meta)
                upstream.settimeout(120)
                if upgrade:
                    header=f'{h.command} {h.path} HTTP/1.1\r\n'
                    for key,value in h.headers.items():
                        if key.lower()!='x-gap-project': header+=key+': '+value+'\r\n'
                    upstream.sendall((header+'\r\n').encode('latin-1'))
                    received=bytearray()
                    while not received.endswith(b'\r\n\r\n'):
                        part=upstream.recv(1)
                        if not part or len(received)>65536: raise VMError('invalid_upgrade_response')
                        received.extend(part)
                    h.connection.sendall(received); sent=True
                    if received.split(b'\r\n')[0].split()[1]!=b'101':
                        upstream.close(); h.close_connection=True; return
                else:
                    body=self.request_body(h)
                    headers={k:v for k,v in h.headers.items() if k.lower() not in HOP|{'x-gap-project','content-length'}}
                    headers['Content-Length']=str(len(body)); headers['Connection']='close'
                    conn=http.client.HTTPConnection('127.0.0.1',port,timeout=120); conn.sock=upstream
                    try:
                        conn.request(h.command,h.path,body=body,headers=headers)
                        response=conn.getresponse()
                        h.send_response_only(response.status,response.reason)
                        for key,value in response.getheaders():
                            if key.lower() not in HOP|{'content-length'}: h.send_header(key,value)
                        # Close-delimited response avoids forwarding stale chunk framing.
                        h.send_header('Connection','close'); h.end_headers(); sent=True
                        if h.command!='HEAD':
                            while True:
                                chunk=response.read(65536)
                                if not chunk: break
                                h.wfile.write(chunk); h.wfile.flush()
                    finally: conn.close()
                    h.close_connection=True
            if upgrade:
                # WebSocket silence counts towards idle, unlike an active HTTP response.
                self.tunnel(meta,h.connection,upstream); h.close_connection=True
        except Exception as error:
            self.last_error_type=type(error).__name__
            if not sent:
                code=402 if str(error)=='microvm_credits_or_budget_exhausted' else 503
                if str(error)=='unknown_application': code=404
                data=json.dumps({'error':str(error) if isinstance(error,VMError) else 'application_gateway_failed'}).encode()
                try:
                    h.send_response(code); h.send_header('Content-Length',str(len(data))); h.send_header('Connection','close'); h.end_headers(); h.wfile.write(data)
                except OSError: pass
            h.close_connection=True
        finally:
            if upstream is not None: upstream.close()
