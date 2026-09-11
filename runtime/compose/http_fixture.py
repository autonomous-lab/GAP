"""Disposable guest application used by the real routing acceptance test."""
import base64
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import uuid


class App(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *args):
        pass

    def do_POST(self):
        self.do_GET()

    def do_GET(self):
        if self.path == '/socket':
            digest = hashlib.sha1((self.headers['Sec-WebSocket-Key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()
            self.send_response(101)
            self.send_header('Upgrade', 'websocket')
            self.send_header('Connection', 'Upgrade')
            self.send_header('Sec-WebSocket-Accept', base64.b64encode(digest).decode())
            self.end_headers()
            header = self.rfile.read(2)
            mask = self.rfile.read(4)
            payload = self.rfile.read(header[1] & 127)
            message = bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
            self.wfile.write(bytes([129, len(message)]) + message)
            self.wfile.flush()
            self.close_connection = True
            return
        if self.path == '/failure':
            self.send_response(503)
            self.send_header('Content-Length', '15')
            self.end_headers()
            self.wfile.write(b'app-unavailable')
            return
        if self.path.startswith('/echo'):
            data = self.rfile.read(int(self.headers.get('Content-Length', 0)))
            body = json.dumps({'path': self.path, 'method': self.command,
                               'body': data.decode(), 'prefix': self.headers.get('X-Forwarded-Prefix')}).encode()
        elif self.path == '/runtime-environment':
            body = json.dumps({k:v for k,v in os.environ.items() if k.startswith(('GAP_', 'TEST_INTERPOLATED_'))}).encode()
        elif self.path == '/asset.txt':
            body = b'nested-asset'
        else:
            body = Path('/persist/id').read_bytes()
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Service-Worker-Allowed', '/')  # gateway must remove broadening
        self.end_headers()
        self.wfile.write(body)


if __name__ == '__main__':
    identity = Path('/persist/id')
    if not identity.exists():
        identity.write_text(str(uuid.uuid4()))
    ThreadingHTTPServer(('0.0.0.0', 8000), App).serve_forever()
