"""Bounded ttyd protocol acceptance; real SSH command supplied by KVM test."""
import base64
import http.client
import json
import os
import shutil
import socket
import threading
import time
import unittest
from http.server import ThreadingHTTPServer
from types import SimpleNamespace
from ttyd_terminal import InputFrames, TtydSession


def frame(payload,opcode=2,fin=True):
    mask=os.urandom(4);n=len(payload)
    header=bytes([(128 if fin else 0)|opcode,128|(n if n<126 else 126)])
    if n>=126:header+=n.to_bytes(2,'big')
    return header+mask+bytes(v^mask[i%4] for i,v in enumerate(payload))


class WebSocket:
    def __init__(self,port,path,token):
        self.sock=socket.create_connection(('127.0.0.1',port),timeout=15)
        host='127.0.0.1:'+str(port)
        key=base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall(('GET '+path+'ws HTTP/1.1\r\nHost: '+host+'\r\nOrigin: http://'+host+'\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: tty\r\nSec-WebSocket-Key: '+key+'\r\nX-GAP-Terminal-Admission: '+token+'\r\n\r\n').encode())
        data=b''
        while not data.endswith(b'\r\n\r\n'):
            b=self.sock.recv(1)
            if not b:raise AssertionError('closed handshake: '+repr(data))
            data+=b
        assert b' 101 ' in data,data
        self.send(json.dumps({'AuthToken':'','columns':80,'rows':24}).encode())
    def send(self,payload):self.sock.sendall(frame(payload))
    def read(self,n):
        result=b''
        while len(result)<n:
            data=self.sock.recv(n-len(result))
            if not data:raise EOFError('websocket closed')
            result+=data
        return result
    def expect(self,marker):
        output=b'';deadline=time.monotonic()+20
        while marker not in output and time.monotonic()<deadline:
            first,second=self.read(2);n=second&127
            if n==126:n=int.from_bytes(self.read(2),'big')
            elif n==127:n=int.from_bytes(self.read(8),'big')
            data=self.read(n)
            if first&15==2 and data[:1]==b'0':output+=data[1:]
            elif first&15==8:raise AssertionError('unexpected close: '+repr(output))
        assert marker in output,output
        return output
    def close(self):self.sock.close()


def acceptance(command):
    from runner import handler_for
    project='prj_'+'a'*24;owner='did:gap:test';token='a'*64
    session=TtydSession(command,(project,owner,'vm_test'),80,24)
    touches=[]
    runner=SimpleNamespace(ingress=SimpleNamespace(admission_token=token),authorize=lambda *a:None,
        hypervisor=SimpleNamespace(read=lambda *a:{}),runtime=SimpleNamespace(touch=lambda m:touches.append(1)),
        terminals=SimpleNamespace(lock=threading.RLock(),sessions={session.id:session},rpc=lambda *a:{'closed':session.closed}))
    server=ThreadingHTTPServer(('127.0.0.1',0),handler_for(runner));server.daemon_threads=True
    thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start();port=server.server_port
    ws=None
    try:
        for secret,status in [('',403),(token,200)]:
            conn=http.client.HTTPConnection('127.0.0.1',port,timeout=10)
            conn.request('GET',session.prefix+'/',headers={'X-GAP-Terminal-Admission':secret})
            response=conn.getresponse();assert response.status==status,response.status
            data=response.read();conn.close()
            if status==200:assert b'<html' in data.lower()
        ws=WebSocket(port,session.prefix+'/',token)
        ws.expect(b'# ' if os.getuid()==0 or command[0]=='ssh' else b'$ ')
        seen=session.seen;typed=session.typed
        ws.send(b'1{"columns":100,"rows":32}');time.sleep(.15)
        assert session.typed==typed and session.seen==seen and not touches
        ws.send(b"0printf '\\107\\101\\120\\055\\124\\124\\131\\104\\055'; stty size\n")
        ws.expect(b'GAP-TTYD-32 100');assert touches and session.seen==seen
        ws.send(b'0sleep 30\n');time.sleep(.15);ws.send(b'0\x03')
        ws.send(b"0printf '\\103\\124\\122\\114\\055\\103\\055\\117\\113\\n'\n");ws.expect(b'CTRL-C-OK')
        ws.close();ws=None;time.sleep(.3)
        ws=WebSocket(port,session.prefix+'/',token)
        ws.send(b"0printf '\\122\\105\\103\\117\\116\\116\\105\\103\\124\\105\\104\\n'\n");ws.expect(b'RECONNECTED')
        session.close();assert session.process.poll() is not None
        conn=http.client.HTTPConnection('127.0.0.1',port,timeout=5)
        conn.request('GET',session.prefix+'/',headers={'X-GAP-Terminal-Admission':token})
        assert conn.getresponse().status==404;conn.close()
        print('ttyd HTTP admission, WebSocket input, resize, Ctrl-C, reconnect, close: OK',flush=True)
    finally:
        if ws:ws.close()
        session.close();server.shutdown();server.server_close();thread.join(timeout=2)


class TtydTests(unittest.TestCase):
    def test_input_not_resize_ping_or_auth(self):
        parser=InputFrames()
        for payload,opcode in [(b'1{"rows":32}',2),(b'0',9),(b'{"AuthToken":""}',2)]:
            self.assertFalse(parser.feed(frame(payload,opcode)))
        data=frame(b'0echo bonjour\n')
        self.assertFalse(parser.feed(data[:4]));self.assertTrue(parser.feed(data[4:]))
        self.assertFalse(parser.feed(frame(b'0',fin=False)))
        self.assertTrue(parser.feed(frame(b'x',opcode=0)))
        with self.assertRaises(ValueError):parser.feed(b'\x82\x01x')
    @unittest.skipUnless(shutil.which('ttyd'),'ttyd is installed in the worker image')
    def test_real_ttyd(self):acceptance(['/bin/sh','-i'])

if __name__=='__main__':unittest.main()
