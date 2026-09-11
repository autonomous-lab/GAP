"""Isolated real-node admin auth tests. No message leaves the local SMTP sink."""
import email
import json
import os
from pathlib import Path
import queue
import re
import socket
import socketserver
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request


def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


class SMTP(socketserver.StreamRequestHandler):
    def handle(self):
        self.wfile.write(b'220 isolated test relay\r\n')
        while line := self.rfile.readline():
            verb = line.split(b' ', 1)[0].strip().upper()
            if verb == b'DATA':
                self.wfile.write(b'354 send body\r\n')
                parts = []
                while (part := self.rfile.readline()) not in (b'.\r\n', b''):
                    parts.append(part)
                self.server.messages.put(b''.join(parts))
                self.wfile.write(b'250 accepted locally\r\n')
            elif verb == b'QUIT':
                self.wfile.write(b'221 bye\r\n')
                return
            else:
                self.wfile.write(b'250 OK\r\n')


@unittest.skipUnless(os.environ.get('GAP_TEST_BINARY'), 'GAP_TEST_BINARY required')
class AdminHTTP(unittest.TestCase):
    def test_password_email_session_origin_csrf_and_inventory(self):
        with tempfile.TemporaryDirectory() as directory, socketserver.ThreadingTCPServer(('127.0.0.1', 0), SMTP) as smtp:
            smtp.messages = queue.Queue()
            threading.Thread(target=smtp.serve_forever, daemon=True).start()
            root = Path(directory)
            (root/'approvals.json').write_text('{"agents":[]}')
            base = 'http://127.0.0.1:' + str(port())
            env = {'PATH': os.environ['PATH'], 'GAP_ADDR': base.removeprefix('http://'),
                   'GAP_STORAGE': 'sqlite', 'GAP_SQLITE_PATH': str(root/'node.sqlite'),
                   'GAP_CLOUD_ROOT': str(root/'projects'), 'GAP_WORKERS': '4',
                   'GAP_MASTER_KEY': 'c'*64, 'GAP_PUBLIC_URL': 'https://client.test',
                   'GAP_CLOUD_ADMIN_ENABLED': '1', 'GAP_ADMIN_EMAILS': 'owner@example.com',
                   'GAP_ADMIN_ORIGIN': 'https://admin.test', 'GAP_ADMIN_DB': str(root/'admin.sqlite'),
                   'GAP_ADMIN_CHALLENGES_DB': str(root/'challenges.sqlite'),
                   'GAP_SMTP_HOST': '127.0.0.1', 'GAP_SMTP_PORT': str(smtp.server_address[1]),
                   'GAP_SMTP_FROM': 'test@example.com', 'GAP_COMPOSE_ENABLED': '1',
                   'GAP_COMPOSE_APPROVALS_FILE': str(root/'approvals.json'),
                   'GAP_COMPOSE_RUNNER_TOKEN': 'b'*64, 'GAP_COMPOSE_RUNNER_URL': 'http://127.0.0.1:9'}
            with (root/'node.log').open('w') as log:
                proc = subprocess.Popen([os.environ['GAP_TEST_BINARY']], env=env, cwd=root, stdout=log, stderr=log)
                try:
                    def request(method, path, body=None, cookie=None, csrf=None, host='admin.test', origin='https://admin.test', bearer=None):
                        headers = {'Host': host, 'Origin': origin, 'Content-Type': 'application/json'}
                        if bearer: headers['Authorization'] = 'Bearer '+bearer
                        if cookie: headers['Cookie'] = cookie
                        if csrf: headers['X-CSRF-Token'] = csrf
                        req = urllib.request.Request(base+path, method=method, headers=headers,
                                                     data=json.dumps(body).encode() if body is not None else None)
                        try: response = urllib.request.urlopen(req, timeout=10)
                        except urllib.error.HTTPError as error: response = error
                        with response:
                            raw = response.read().decode()
                            try: value = json.loads(raw)
                            except ValueError: value = raw
                            return response.status, value, response.headers
                    for _ in range(200):
                        try:
                            if request('GET', '/health')[0] == 200: break
                        except OSError: pass
                        if proc.poll() is not None: self.fail('Isolated node failed: '+(root/'node.log').read_text())
                        time.sleep(.05)
                    api = '/v1/admin/console/'
                    self.assertEqual(request('GET', '/admin')[0], 200)
                    self.assertEqual(request('GET', '/apps/tenant')[0], 404)
                    self.assertEqual(request('POST', '/v1/identity')[0], 404)
                    self.assertEqual(request('GET', api+'session')[0], 401)
                    self.assertEqual(request('POST', api+'login', {}, origin='https://client.test')[0], 403)
                    self.assertEqual(request('GET', api+'session', host='client.test')[0], 403)
                    credentials = {'email':'owner@example.com','password':'unique strong admin password'}
                    status, challenge, _ = request('POST', api+'login', credentials)
                    self.assertEqual(status, 202)
                    self.assertNotIn('code', challenge)
                    msg = email.message_from_bytes(smtp.messages.get(timeout=5))
                    body = msg.get_payload(decode=True).decode()
                    code = re.search(r'\b\d{6}\b', body).group()
                    status, session, headers = request('POST', api+'verify', {'challenge_id':challenge['challenge_id'],'code':code})
                    self.assertEqual(status, 200)
                    cookie = headers['Set-Cookie']
                    for flag in ['Secure', 'HttpOnly', 'SameSite=Strict', 'Path=/']:
                        self.assertIn(flag, cookie)
                    self.assertNotIn('Domain=', cookie)
                    cookie = cookie.split(';')[0]
                    self.assertEqual(request('POST', api+'verify', {'challenge_id':challenge['challenge_id'],'code':code})[0], 400)
                    self.assertEqual(request('GET', api+'session', cookie=cookie)[0], 200)
                    self.assertEqual(request('GET', api+'overview', cookie=cookie)[0], 200)
                    self.assertEqual(request('GET', api+'agents', cookie=cookie)[1]['agents'], [])
                    _, agent, _ = request('POST', '/v1/identity', host='client.test')
                    _, project, _ = request('POST', '/v1/cloud/projects', {}, host='client.test', bearer=agent['token'])
                    project_id = project['project_id']
                    route = '/v1/cloud/projects/'+project_id+'/access-requests'
                    status, access, _ = request('POST', route, {'request_id':'d'*32,'quota':{'vcpus':2,'memory_mib':4096,'max_vms':1},'always_on':False,'reason':'Test workload'}, host='client.test', bearer=agent['token'])
                    self.assertEqual(status, 202)
                    status, listed, _ = request('GET', api+'approvals', cookie=cookie)
                    self.assertEqual(status, 200)
                    self.assertEqual(listed['requests'][0]['id'], access['id'])
                    self.assertEqual(request('POST', api+'approvals/'+access['id'], {'approve':True}, cookie=cookie)[0], 403)
                    status, reviewed, _ = request('POST', api+'approvals/'+access['id'], {'approve':True}, cookie=cookie, csrf=session['csrf'])
                    self.assertEqual(status, 200)
                    self.assertEqual(reviewed['status'], 'approved')
                    self.assertIn(agent['did'], json.loads((root/'approvals.json').read_text())['agents'])
                    self.assertNotIn(agent['token'], json.dumps(request('GET', api+'agents', cookie=cookie)[1]))
                    self.assertEqual(request('GET', api+'projects/'+project_id, cookie=cookie)[0], 200)
                    self.assertEqual(request('POST', api+'logout', {}, cookie=cookie)[0], 403)
                    self.assertEqual(request('POST', api+'logout', {}, cookie=cookie, csrf=session['csrf'], origin='https://client.test')[0], 403)
                    self.assertEqual(request('POST', api+'login', {**credentials,'password':'different wrong password'})[0], 401)
                    self.assertEqual(request('POST', api+'logout', {}, cookie=cookie, csrf=session['csrf'])[0], 200)
                    self.assertEqual(request('GET', api+'session', cookie=cookie)[0], 401)
                finally:
                    proc.terminate()
                    proc.wait(timeout=10)
                    smtp.shutdown()


if __name__ == '__main__':
    unittest.main()
