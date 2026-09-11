"""Real GAP HTTP -> runner HTTP -> GAP authorization.

Run after cargo build: GAP_TEST_BINARY=target/debug/gap python3 .../integration_test.py
Guests are simulated by default. GAP_VM_TEST_ALLOW_CREATE=1 plus isolated state
and image paths enables disposable real KVM/Compose tests (see README.md).
"""
import base64
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import unittest
import uuid
from urllib.parse import urlsplit
import urllib.error
import urllib.request
from http.server import ThreadingHTTPServer

from runner import Runner, handler_for


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@unittest.skipUnless(os.environ.get("GAP_TEST_BINARY"), "GAP_TEST_BINARY required")
class Integration(unittest.TestCase):
    def test_private_end_to_end_control_plane(self):
        self.exercise(True)

    def test_public_end_to_end_control_plane(self):
        self.exercise(False)

    def exercise(self, private):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            approval = root / "agents.json"
            approval.write_text('{"agents":[]}')
            compose_approval = root / "compose-agents.json"
            compose_approval.write_text('{"agents":[]}')
            admin, service = "a" * 64, "b" * 64
            node_port, runner_port = free_port(), free_port()
            node_url = f"http://127.0.0.1:{node_port}"
            env = {"PATH": os.environ["PATH"], "GAP_ADDR": f"127.0.0.1:{node_port}",
                   "GAP_STORAGE": "sqlite", "GAP_SQLITE_PATH": str(root / "node.sqlite"),
                   "GAP_CLOUD_ROOT": str(root / "projects"), "GAP_WORKERS": "4",
                   "GAP_PRIVATE_NODE": "1" if private else "0", "GAP_PRIVATE_APPROVALS_FILE": str(approval),
                   "GAP_COMPOSE_APPROVALS_FILE": str(compose_approval),
                   "GAP_ADMIN_TOKEN": admin, "GAP_COMPOSE_ENABLED": "1",
                   "GAP_COMPOSE_RUNNER_TOKEN": service,
                   "GAP_COMPOSE_RUNNER_URL": f"http://127.0.0.1:{runner_port}"}
            process = subprocess.Popen([str(Path(os.environ["GAP_TEST_BINARY"]).resolve())], env=env,
                                       cwd=root, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            server = None
            caddy_process = None
            try:
                def request(method, path, token=None, body=None):
                    headers = {"Content-Type": "application/json"}
                    if token:
                        headers["Authorization"] = "Bearer " + token
                    req = urllib.request.Request(node_url + path, method=method, headers=headers,
                                                 data=json.dumps(body).encode() if body is not None else None)
                    try:
                        response = urllib.request.urlopen(req, timeout=10)
                    except urllib.error.HTTPError as error:
                        response = error
                    with response:
                        return response.status, json.loads(response.read())
                for _ in range(100):
                    try:
                        if request("GET", "/health")[0] == 200:
                            break
                    except OSError:
                        pass
                    if process.poll() is not None:
                        self.fail("GAP exited during private bootstrap")
                    time.sleep(.05)
                self.assertEqual(request("POST", "/v1/identity")[0], 401 if private else 200)
                status, identity = request("POST", "/v1/identity", admin)
                self.assertEqual(status, 200)
                token, owner = identity["token"], identity["did"]
                _, other = request("POST", "/v1/identity", admin)
                self.assertEqual(request("POST", "/v1/cloud/projects", token)[0], 401 if private else 200)
                approval.write_text(json.dumps({"agents": [owner, other["did"]]}))
                status, project = request("POST", "/v1/cloud/projects", token)
                self.assertEqual(status, 200)
                project_id = project["project_id"]
                (root / "token").write_text(service)
                config = {"approved_only": True, "token_file": str(root / "token"),
                          "state_dir": str(root / "runner"), "node_url": node_url,
                          "guests": {project_id: {"microvm": True, "vm_id": "test-guest",
                              "owner_did": owner, "expires_at": int(time.time()) + 3600,
                              "address": "127.0.0.1", "port": 2222,
                              "ssh_key": str(root / "unused-test-key"),
                              "known_hosts": str(root / "unused-host-key")}}}
                managed = os.environ.get("GAP_VM_TEST_ALLOW_CREATE") == "1"
                if managed:
                    if os.environ.get('GAP_TEST_SERVERLESS')=='1':
                        (root/'operator-token').write_text('o'*64)
                        config.update(serverless=True,operator_token_file=str(root/'operator-token'),wake_gateway_port=free_port())
                    config.pop("guests")
                    config["hypervisor"] = {"state_dir": str(Path(os.environ["GAP_VM_TEST_STATE_DIR"])/project_id),
                                            "image_dir": os.environ["GAP_VM_TEST_IMAGE_DIR"],
                                            "public_network": {"hostname": "sites.gap.geta.team", "first_port": 24100, "last_port": 24109}}
                if managed and os.environ.get('GAP_TEST_CADDY_BINARY'):
                    config['ingress'] = {'dedicated_caddy': True, 'public_url': os.environ.get('GAP_TEST_APP_ORIGIN', 'http://127.0.0.1:8093'),
                        'admin_socket': str(root / 'caddy-admin.sock'),
                        'http_port': int(os.environ.get('GAP_TEST_APP_PORT', '8093'))}
                    bootstrap = root / 'caddy.json'
                    bootstrap.write_text(json.dumps({'admin': {'listen': 'unix/' + config['ingress']['admin_socket']}}))
                    caddy_process = subprocess.Popen([os.environ['GAP_TEST_CADDY_BINARY'], 'run', '--config', str(bootstrap)],
                        env={'PATH': os.environ['PATH'], 'XDG_DATA_HOME': str(root / 'caddy-data'),
                             'XDG_CONFIG_HOME': str(root / 'caddy-config'), 'HOME': str(root)},
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                    for _ in range(100):
                        try:
                            with socket.socket(socket.AF_UNIX) as probe:
                                probe.connect(config['ingress']['admin_socket'])
                            break
                        except OSError:
                            if caddy_process.poll() is not None:
                                self.fail('test Caddy exited')
                            time.sleep(.05)
                config_path = root / "runner.json"
                config_path.write_text(json.dumps(config))
                executions = []
                def execute(vm, payload):
                    executions.append(payload)
                    return {"ok": True, "output": "simulated guest"}
                from runner import execute_guest
                runner = Runner(config_path, execute=execute_guest if managed else execute)
                server = ThreadingHTTPServer(("127.0.0.1", runner_port), handler_for(runner))
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                prefix = f"/v1/cloud/projects/{project_id}/stack"
                bundle = {"request_id": "c" * 32, "compose_file": "compose.yaml",
                          "files": {"compose.yaml": base64.b64encode(b"services: {}").decode()}}
                self.assertEqual(request("POST", prefix + "/releases", None, bundle)[0], 401)
                self.assertEqual(request("POST", prefix + "/releases", other["token"], bundle)[0], 401)
                self.assertEqual(request("POST", prefix + "/releases", token, bundle)[0], 401)
                self.assertFalse(executions)
                compose_approval.write_text(json.dumps({"agents": [owner]}))
                if managed:
                    self.exercise_managed(request, prefix, token, other["token"], runner, project_id, owner, compose_approval, root)
                    return
                status, job = request("POST", prefix + "/releases", token, bundle)
                self.assertEqual(status, 202, job)
                job_path = prefix + "/jobs/" + job["job_id"]
                for _ in range(100):
                    status, result = request("GET", job_path, token)
                    if result.get("status") == "succeeded":
                        break
                    time.sleep(.02)
                self.assertEqual(result["status"], "succeeded", result)
                retry = request("POST", prefix + "/releases", token, bundle)
                self.assertEqual(retry[0], 200)
                self.assertEqual(retry[1]["job_id"], job["job_id"])
                self.assertEqual(len(executions), 1)
                self.assertEqual(request("GET", job_path, other["token"])[0], 401)
                # A live callback through the same node proved forward() did
                # not keep the global lock across worker I/O.
                self.assertEqual(request("GET", "/health")[0], 200)
                self.assertEqual(request("POST", "/internal/compose/authorize", "wrong",
                                         {"project_id": project_id, "owner_did": owner})[0], 403)
                compose_approval.write_text('{"agents":[]}')
                self.assertEqual(request("GET", job_path, token)[0], 401)
                self.assertEqual(request("GET", "/v1/cloud/projects", token)[0], 200)
                self.assertEqual(request("POST", "/internal/compose/authorize", service,
                                         {"project_id": project_id, "owner_did": owner})[0], 403)
                approval.write_text('{"agents":[]}')
                self.assertEqual(request("GET", "/v1/cloud/projects", token)[0], 401 if private else 200)
            finally:
                if 'runner' in locals() and runner.runtime:
                    runner.runtime.closed=True
                    runner.runtime.gateway.http_server.shutdown()
                    runner.runtime.gateway.http_server.server_close()
                    for project in list(runner.runtime.locks): runner.runtime.gateway.withdraw(project)
                if caddy_process:
                    caddy_process.terminate()
                    caddy_process.wait(timeout=10)
                if server:
                    server.shutdown()
                    server.server_close()
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()

    def exercise_managed(self, request, prefix, token, other_token, runner, project, owner, approval, root):
        legacy_request = request
        def request(method, path, token=None, body=None):
            if path.startswith(prefix + '/'):
                suffix = path[len(prefix):]
                if suffix == '/vm' or suffix.startswith('/vm/'):
                    path = prefix[:-len('/stack')] + suffix
                elif suffix in ('/ports', '/ssh', '/ingress') or suffix.startswith('/jobs/'):
                    path = prefix[:-len('/stack')] + '/vm' + suffix
            return legacy_request(method, path, token, body)

        def operation(method, path, body=None, expected_error=None):
            body = {"request_id": uuid.uuid4().hex, **(body or {})}
            status, job = request(method, prefix + path, token, body)
            self.assertEqual(status, 202, job)
            retry_status, retry = legacy_request(method, prefix + path, token, body)
            self.assertEqual(retry_status, 200, retry)
            self.assertEqual(job["job_id"], retry["job_id"])
            deadline = time.monotonic() + 600
            while time.monotonic() < deadline:
                _, result = request("GET", prefix + "/jobs/" + job["job_id"], token)
                if result["status"] not in ("queued", "running"):
                    self.assertEqual(result["status"], "failed" if expected_error else "succeeded", result)
                    if expected_error:
                        self.assertEqual(result["result"]["error"], expected_error)
                    return result["result"]
                time.sleep(.2)
            self.fail("managed job timeout")
        manager = runner.hypervisor
        try:
            self.assertEqual(request("GET", prefix + "/vm", token)[1]["state"], "absent")
            self.assertEqual(request("POST", prefix + "/vm", other_token, {"request_id": uuid.uuid4().hex})[0], 401)
            initial = request("GET", prefix + "/vm", token)[1]['agent_quota']
            self.assertEqual(initial, {'limits': {'vcpus': 2, 'memory_mib': 4096}, 'allocated': {'vcpus': 0, 'memory_mib': 0}})
            operation('POST', '/vm', {'vcpus': 3}, expected_error='agent_quota_exceeded_vcpus')
            approval.write_text(json.dumps({'agents': [owner], 'quotas': {owner: {'vcpus': 1, 'memory_mib': 512}}}))
            operation('POST', '/vm', {}, expected_error='agent_quota_exceeded_memory_mib')
            approval.write_text(json.dumps({'agents': [owner]}))
            print('LIVE AGENT QUOTA DEFAULTS + OVERRIDES + REJECTION BEFORE CREATE OK', flush=True)
            vm = operation("POST", "/vm", {"vcpus": 1, "memory_mib": 1024, "disk_gib": 4, "ports": [8000]})["vm"]
            identity = {"vm_id": vm["vm_id"]}
            from runner import execute_guest
            def ready():
                deadline = time.monotonic() + 180
                while time.monotonic() < deadline:
                    try:
                        if execute_guest(manager.guest(project, owner), {"action": "vm_probe", "body": {}}, timeout=12).get("ok"):
                            return
                    except Exception:
                        pass
                    time.sleep(1)
                self.fail("guest Docker unavailable")
            ready()
            public_ports = request('GET', prefix+'/ports', token)[1]['ports']
            self.assertEqual(len(public_ports), 5)
            self.assertEqual(request('GET', prefix+'/ports', other_token)[0], 401)
            key = root/'agent-key'
            subprocess.run(['ssh-keygen','-q','-t','ed25519','-N','','-f',str(key)], check=True)
            operation('PUT', '/ssh', {**identity, 'authorized_keys': [Path(str(key)+'.pub').read_text().strip()]})
            def guest_env(command='env'):
                meta = manager.read(project, owner)
                result = subprocess.run(['ssh','-F','/dev/null','-o','BatchMode=yes',
                    '-o','IdentitiesOnly=yes','-o','StrictHostKeyChecking=yes',
                    '-o','UserKnownHostsFile='+str(manager.folder(meta)/'known_hosts'),
                    '-o','HostKeyAlias='+meta['vm_id'],'-i',str(key),'-p',str(meta['ssh_port']),
                    'root@127.0.0.1', command], capture_output=True, text=True, check=True, timeout=15)
                return dict(line.split('=',1) for line in result.stdout.splitlines() if line.startswith('GAP_'))
            initial_env = guest_env()
            self.assertEqual(initial_env['GAP_HTTP_PORT'], '')
            self.assertEqual(initial_env['GAP_PUBLIC_PORTS'], ','.join(str(p['public_port']) for p in public_ports))
            self.assertEqual(initial_env, guest_env('gap-env env'))
            self.assertEqual(initial_env['GAP_VM_ID'], guest_env("su nobody -s /bin/sh -c 'gap-env env'")['GAP_VM_ID'])
            ssh_info = request('GET', prefix+'/ssh', token)[1]
            self.assertTrue(ssh_info['host_key_fingerprint'].startswith('SHA256:'))
            self.assertEqual(len(ssh_info['authorized_keys']), 1)
            operation('PUT', '/ports', {**identity, 'mappings': [{'slot':1,'guest_port':22,'protocol':'tcp'}]})
            self.assertTrue(request('GET', prefix+'/ports', token)[1]['ports'][0]['routed'])
            self.assertEqual(request('GET', prefix+'/ssh', other_token)[0], 401)
            operation('PUT', '/ports', {**identity, 'mappings': []})
            self.assertFalse(request('GET', prefix+'/ports', token)[1]['ports'][0]['routed'])
            print('REAL GAP HTTP: PORT RESERVATION + HOT MAPPINGS + SSH KEYS + OWNER ISOLATION OK', flush=True)
            if runner.ingress:
                operation('PUT', '/ingress', {**identity, 'enabled': True, 'guest_port': 8000})
                self.assertEqual(guest_env()['GAP_HTTP_PORT'], '8000')
                # No Compose release exists. Native HTTP still runs with Docker stopped.
                guest_env('rc-service docker stop')
                self.assertFalse(execute_guest(manager.guest(project, owner), {'action':'vm_probe','body':{}}, timeout=12)['ok'])
                guest_env("mkdir -p /root/native-www; echo native-without-docker > /root/native-www/index.html; nohup gap-env sh -c 'exec python3 -m http.server \"$GAP_HTTP_PORT\" --bind 0.0.0.0 --directory /root/native-www' >/tmp/native.log 2>&1 </dev/null & echo $! > /tmp/native.pid")
                native_url = request('GET', prefix+'/ingress', token)[1]['url']
                for attempt in range(40):
                    try:
                        with urllib.request.urlopen(native_url, timeout=5) as response:
                            self.assertEqual(response.read().strip(), b'native-without-docker')
                        break
                    except OSError:
                        if attempt == 39:
                            raise
                        time.sleep(.25)
                guest_env('kill "$(cat /tmp/native.pid)"; rc-service docker start')
                ready()
                print('CANONICAL VM API + LEGACY DEDUP + NATIVE HTTP WITHOUT DOCKER OK', flush=True)
            sources = {
                "compose.yaml": 'services:\n  web:\n    build: .\n    ports: ["8000:8000"]\n    env_file:\n      - path: /etc/gap/runtime.env\n        format: raw\n    environment:\n      TEST_INTERPOLATED_HTTP_PORT: ${GAP_HTTP_PORT}\n    volumes: ["data:/persist"]\nvolumes:\n  data: {}\n',
                "Dockerfile": 'FROM alpine:3.23\nRUN apk add --no-cache python3\nCOPY http_fixture.py /app.py\nCMD ["python3", "/app.py"]\n',
                "http_fixture.py": (Path(__file__).parent / 'http_fixture.py').read_text()}
            operation("POST", "/releases", {"compose_file": "compose.yaml", "files": {
                name: base64.b64encode(value.encode()).decode() for name, value in sources.items()}})
            url = 'http://127.0.0.1:' + str(vm["ports"][0]["worker_port"])
            def page():
                with urllib.request.urlopen(url, timeout=10) as response:
                    return response.read().strip()
            persisted = page()
            if runner.ingress:
                ingress = operation('PUT', '/ingress', {**identity, 'enabled': True, 'guest_port': 8000})['ingress']
                self.assertTrue(ingress['routed'])
                def app_request(path='', method='GET', data=None):
                    req = urllib.request.Request(ingress['url'] + path, method=method, data=data)
                    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req, timeout=10) as response:
                        self.assertIsNone(response.headers.get('Service-Worker-Allowed'))
                        return response.read().strip()
                self.assertEqual(app_request(), persisted)
                app_env = json.loads(app_request('runtime-environment'))
                self.assertEqual(app_env['GAP_HTTP_PORT'], '8000')
                self.assertEqual(app_env['TEST_INTERPOLATED_HTTP_PORT'], '8000')
                self.assertEqual(app_env['GAP_PUBLIC_URL'], ingress['url'])
                self.assertEqual(app_env['GAP_PUBLIC_HOST'], 'sites.gap.geta.team')
                self.assertEqual(len(json.loads(app_env['GAP_PORTS_JSON'])), 5)
                print('REAL GUEST -> COMPOSE INTERPOLATION + CONTAINER ENVIRONMENT OK', flush=True)
                echo = json.loads(app_request('echo?value=a%2Fb&x=1', 'POST', b'hello-world'))
                self.assertEqual(echo['path'], '/echo?value=a%2Fb&x=1')
                self.assertEqual(echo['method'], 'POST')
                self.assertEqual(echo['body'], 'hello-world')
                self.assertEqual(echo['prefix'], '/apps/' + project)
                self.assertEqual(app_request('asset.txt'), b'nested-asset')
                with self.assertRaises(urllib.error.HTTPError) as failure:
                    app_request('failure')
                self.assertEqual(failure.exception.code, 503)
                self.assertEqual(failure.exception.read(), b'app-unavailable')
                # Missing slash redirects to the canonical path, preserving query.
                with urllib.request.urlopen(ingress['url'][:-1] + '?probe=1', timeout=10) as response:
                    self.assertEqual(response.geturl(), ingress['url'] + '?probe=1')
                origin = urlsplit(ingress['url'])
                with socket.create_connection((origin.hostname, origin.port or 80), timeout=5) as ws:
                    path = origin.path + 'socket'
                    ws.sendall(('GET ' + path + ' HTTP/1.1\r\nHost: ' + origin.netloc +
                        '\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13'
                        '\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n').encode())
                    wire = ws.makefile('rb')
                    self.assertIn(b'101', wire.readline())
                    for _ in range(64):
                        if wire.readline() == b'\r\n':
                            break
                    else:
                        self.fail('invalid WebSocket handshake headers')
                    ws.sendall(b'\x81\x84abcd' + bytes(c ^ b'abcd'[i % 4] for i, c in enumerate(b'ping')))
                    self.assertEqual(wire.read(6), b'\x81\x04ping')
                    wire.close()
                print('REAL SHARED-ORIGIN PATH: GET + POST + QUERY + ASSET + REDIRECT + WEBSOCKET OK', flush=True)
                operation('PUT', '/ingress', {**identity, 'enabled': False})
                self.assertEqual(guest_env()['GAP_INGRESS_ENABLED'], '0')
                self.assertEqual(guest_env()['GAP_HTTP_PORT'], '')
                self.assertFalse(request('GET', prefix + '/ingress', token)[1]['routed'])
                with self.assertRaises(urllib.error.HTTPError) as disabled:
                    app_request()
                self.assertEqual(disabled.exception.code, 404)
                operation('PUT', '/ingress', {**identity, 'enabled': True, 'guest_port': 8000})
            self.assertEqual(len(persisted), 36)
            operation("POST", "/vm/stop", identity)
            if runner.ingress:
                self.assertFalse(request('GET', prefix + '/ingress', token)[1]['routed'])
            self.assertEqual(request('GET', prefix+'/vm', token)[1]['agent_quota']['allocated'], {'vcpus': 1, 'memory_mib': 1024})
            approval.write_text(json.dumps({'agents': [owner], 'quotas': {owner: {'vcpus': 1, 'memory_mib': 4096}}}))
            operation('PATCH', '/vm', {**identity, 'vcpus': 2}, expected_error='agent_quota_exceeded_vcpus')
            approval.write_text(json.dumps({'agents': [owner], 'quotas': {owner: {'vcpus': 2, 'memory_mib': 4096}}}))
            operation("PATCH", "/vm", {**identity, "vcpus": 2, "memory_mib": 1280, "disk_gib": 5})
            operation("POST", "/vm/start", identity)
            ready()
            if runner.ingress:
                self.assertEqual(guest_env()['GAP_HTTP_PORT'], '8000')
            operation("POST", "/start")
            # Compose start reports process start, not HTTP readiness. The Python
            # test service needs a moment to bind its listener after restart.
            for attempt in range(60):
                try:
                    restored = page()
                    break
                except OSError:
                    if attempt == 59:
                        raise
                    time.sleep(.25)
            self.assertEqual(restored, persisted, "named volume must survive VM stop/resize/restart")
            if runner.ingress:
                self.assertEqual(app_request(), persisted)
            operation("POST", "/status")
            operation("POST", "/logs")
            operation("POST", "/vm/stop", identity)
            operation("DELETE", "/vm", {**identity, "delete_data": True, "confirm_data_loss": True})
            self.assertEqual(request("GET", prefix + "/vm", token)[1]["state"], "destroyed")
            self.assertFalse(manager.folder(manager.read(project, owner)).exists())
            if runner.ingress:
                self.assertFalse(request('GET', prefix + '/ingress', token)[1]['routed'])
            approval.write_text('{"agents":[]}')
            self.assertEqual(request("POST", prefix + "/vm", token, {"request_id": uuid.uuid4().hex})[0], 401)
            self.assertEqual(request('GET', prefix+'/ports', token)[0], 401)
            self.assertEqual(request('GET', prefix+'/ssh', token)[0], 401)
            print("REAL GAP HTTP + APPROVAL + KVM + COMPOSE BUILD + HTTP + PERSISTENCE + RESIZE + DELETE: OK", flush=True)
        finally:
            meta = manager.read(project, owner)
            if meta and meta['state'] != 'destroyed':
                if manager.alive(meta):
                    manager.perform(project, owner, 'vm/stop', {'vm_id': meta['vm_id'], 'force': True})
                manager.perform(project, owner, 'vm/destroy', {'vm_id': meta['vm_id'], 'delete_data': True, 'confirm_data_loss': True})


if __name__ == "__main__":
    unittest.main()
