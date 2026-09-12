"""Private ttyd-to-SSH sessions, admitted by the isolated management gateway."""
import hmac
import os
import re
import secrets
import select
import socket
import subprocess
import threading
import time


class TtydSession:
    def __init__(self,command,identity,cols,rows):
        self.identity=identity;self.id=secrets.token_hex(24)
        self.created=self.seen=self.typed=time.monotonic()
        self.closed=False;self.reason=None;self.lock=threading.RLock()
        self.prefix='/v1/cloud/projects/'+identity[0]+'/vm/t/'+self.id
        with socket.socket() as sock:
            sock.bind(('127.0.0.1',0));self.port=sock.getsockname()[1]
        self.process=subprocess.Popen(['ttyd','-i','127.0.0.1','-p',str(self.port),'-W','-O','-m','1',
            '-H','X-GAP-Terminal-User','-b',self.prefix,'-t','titleFixed=GAP terminal',
            '-t','fontSize=14','--',*command],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,
            start_new_session=True,env={'PATH':'/usr/bin:/bin','LANG':'C.UTF-8','TERM':'xterm-256color'})
        try:
            for _ in range(50):
                if self.process.poll() is not None:raise OSError('ttyd_start_failed')
                try:
                    with socket.create_connection(('127.0.0.1',self.port),timeout=.1):return
                except OSError:time.sleep(.02)
            raise OSError('ttyd_start_timeout')
        except BaseException:self.close('ttyd_start_failed');raise

    def close(self,reason='closed'):
        with self.lock:
            if self.closed:return
            self.closed=True;self.reason=reason
            if self.process.poll() is None:
                # ttyd owns its SSH children; terminate the whole private group.
                import signal
                try:os.killpg(self.process.pid,signal.SIGTERM)
                except ProcessLookupError:pass
                try:self.process.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    try:os.killpg(self.process.pid,signal.SIGKILL)
                    except ProcessLookupError:pass
                    self.process.wait(timeout=1)

    def exchange(self,body):raise ValueError('ttyd_requires_websocket')


class InputFrames:
    """Inspect masked browser frames to count typing, not resize or keepalive."""
    def __init__(self):self.buffer=bytearray();self.input=False
    def feed(self,data):
        self.buffer.extend(data);typed=False
        if len(self.buffer)>262144:raise ValueError('terminal_frame_too_large')
        while len(self.buffer)>=2:
            first,second=self.buffer[:2];size=second&127;offset=2
            if not second&128:raise ValueError('unmasked_browser_frame')
            if size==126:
                if len(self.buffer)<4:break
                size=int.from_bytes(self.buffer[2:4],'big');offset=4
            elif size==127:
                if len(self.buffer)<10:break
                size=int.from_bytes(self.buffer[2:10],'big');offset=10
            if size>65536:raise ValueError('terminal_frame_too_large')
            if len(self.buffer)<offset+4+size:break
            mask=self.buffer[offset:offset+4];offset+=4
            payload=bytes(c^mask[i%4] for i,c in enumerate(self.buffer[offset:offset+size]))
            opcode=first&15
            if opcode==2:self.input=payload[:1]==b'0';typed|=self.input and len(payload)>1
            elif opcode==0:typed|=self.input and bool(payload)
            if first&128 and opcode in (0,1,2):self.input=False
            del self.buffer[:offset+size]
        return typed


def proxy(handler,runner):
    token=getattr(runner.ingress,'admission_token',None)
    supplied=handler.headers.get('X-GAP-Terminal-Admission','')
    if not token or not hmac.compare_digest(token,supplied):
        handler.reply(403,{'error':{'code':'terminal_gateway_required'}});return
    match=re.fullmatch(r'/v1/cloud/projects/(prj_[0-9a-f]{24})/vm/t/([0-9a-f]{48})/(?:[^?#]*)?(?:\?[^#]*)?',handler.path)
    if not match or not runner.terminals:
        handler.reply(404,{'error':{'code':'terminal_not_found'}});return
    project,session_id=match.groups()
    with runner.terminals.lock:session=runner.terminals.sessions.get(session_id)
    if not isinstance(session,TtydSession) or session.closed or session.identity[0]!=project:
        handler.reply(404,{'error':{'code':'terminal_not_found'}});return
    # Gate rechecks owner approval, credit and workload policy for every new
    # HTTP/WS connection; the separate authenticated browser pulse limits streams.
    runner.authorize(project,session.identity[1])
    result=runner.terminals.rpc(project,session.identity[1],'authorize',{'terminal_id':session_id})
    if result['closed']:
        handler.reply(403,{'error':{'code':'terminal_closed'}});return
    upstream=socket.create_connection(('127.0.0.1',session.port),timeout=3)
    handler.close_connection=True
    try:
        headers=['GET '+handler.path+' HTTP/1.1','Host: '+handler.headers.get('Host','localhost'),
                 'X-GAP-Terminal-User: gap-session','Connection: '+('Upgrade' if handler.headers.get('Upgrade','').lower()=='websocket' else 'close')]
        for name in ('Upgrade','Sec-WebSocket-Key','Sec-WebSocket-Version','Sec-WebSocket-Protocol','Origin','Accept','Accept-Encoding'):
            value=handler.headers.get(name)
            if value:headers.append(name+': '+value)
        upstream.sendall(('\r\n'.join(headers)+'\r\n\r\n').encode())
        header=bytearray()
        while not header.endswith(b'\r\n\r\n'):
            data=upstream.recv(1)
            if not data or len(header)>32768:raise OSError('invalid_ttyd_response')
            header.extend(data)
        upgraded=header.startswith(b'HTTP/1.1 101')
        extra=b'Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: frame-ancestors \'self\'\r\n'
        handler.connection.sendall(bytes(header[:-2])+extra+b'\r\n')
        frames=InputFrames();handler.connection.settimeout(5);upstream.settimeout(5)
        while not session.closed:
            readers=[upstream,handler.connection] if upgraded else [upstream]
            ready,_,_=select.select(readers,[],[],1)
            for src in ready:
                data=src.recv(65536)
                if not data:return
                if src is handler.connection:
                    if frames.feed(data):
                        session.typed=time.monotonic()
                        meta=runner.hypervisor.read(project,session.identity[1],session.identity[2])
                        runner.runtime.touch(meta)
                    upstream.sendall(data)
                else:handler.connection.sendall(data)
    finally:upstream.close()
