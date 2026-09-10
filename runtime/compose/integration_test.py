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
import ssl
from unittest.mock import patch
import subprocess
import tempfile
import threading
import time
import unittest
import uuid
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
                    config.pop("guests")
                    config["hypervisor"] = {"state_dir": os.environ["GAP_VM_TEST_STATE_DIR"],
                                            "image_dir": os.environ["GAP_VM_TEST_IMAGE_DIR"]}
                if managed and os.environ.get('GAP_TEST_CADDY_BINARY'):
                    config['ingress'] = {'dedicated_caddy': True, 'base_domain': 'apps.test',
                        'admin_socket': str(root / 'caddy-admin.sock'),
                        'http_port': free_port(), 'https_port': free_port(), 'internal_tls': True}
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
        def operation(method, path, body=None):
            body = {"request_id": uuid.uuid4().hex, **(body or {})}
            status, job = request(method, prefix + path, token, body)
            self.assertEqual(status, 202, job)
            retry_status, retry = request(method, prefix + path, token, body)
            self.assertEqual(retry_status, 200, retry)
            self.assertEqual(job["job_id"], retry["job_id"])
            deadline = time.monotonic() + 600
            while time.monotonic() < deadline:
                _, result = request("GET", prefix + "/jobs/" + job["job_id"], token)
                if result["status"] not in ("queued", "running"):
                    self.assertEqual(result["status"], "succeeded", result)
                    return result["result"]
                time.sleep(.2)
            self.fail("managed job timeout")
        manager = runner.hypervisor
        try:
            self.assertEqual(request("GET", prefix + "/vm", token)[1]["state"], "absent")
            self.assertEqual(request("POST", prefix + "/vm", other_token, {"request_id": uuid.uuid4().hex})[0], 401)
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
            sources = {
                "compose.yaml": 'services:\n  web:\n    build: .\n    ports: ["8000:8000"]\n    volumes: ["data:/persist"]\nvolumes:\n  data: {}\n',
                "Dockerfile": 'FROM alpine:3.23\nRUN apk add --no-cache busybox-extras\nCMD ["sh", "-c", "test -f /persist/index.html || cat /proc/sys/kernel/random/uuid > /persist/index.html; exec busybox-extras httpd -f -p 8000 -h /persist"]\n'}
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
                ca = root / 'caddy-data/caddy/pki/authorities/local/root.crt'
                original_resolve = socket.getaddrinfo
                def resolve(host, *args, **kwargs):
                    return original_resolve('127.0.0.1' if host == ingress['hostname'] else host, *args, **kwargs)
                def https_page():
                    context = ssl.create_default_context(cafile=str(ca))
                    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=context))
                    with patch('socket.getaddrinfo', side_effect=resolve):
                        with opener.open(ingress['url'], timeout=5) as response:
                            return response.read().strip()
                for attempt in range(60):
                    try:
                        self.assertEqual(https_page(), persisted)
                        break
                    except (OSError, urllib.error.URLError):
                        if attempt == 59:
                            raise
                        time.sleep(.5)
                print('REAL CADDY HTTPS: certificate chain + hostname verified', flush=True)
                operation('PUT', '/ingress', {**identity, 'enabled': False})
                self.assertFalse(request('GET', prefix + '/ingress', token)[1]['routed'])
                with patch('socket.getaddrinfo', side_effect=resolve):
                    try:
                        https_page()
                    except (OSError, urllib.error.URLError):
                        pass
                    else:
                        self.fail('disabled route still serves application')
                operation('PUT', '/ingress', {**identity, 'enabled': True, 'guest_port': 8000})
            self.assertEqual(len(persisted), 36)
            operation("POST", "/vm/stop", identity)
            if runner.ingress:
                self.assertFalse(request('GET', prefix + '/ingress', token)[1]['routed'])
            operation("PATCH", "/vm", {**identity, "vcpus": 2, "memory_mib": 1280, "disk_gib": 5})
            operation("POST", "/vm/start", identity)
            ready()
            operation("POST", "/start")
            self.assertEqual(page(), persisted, "named volume must survive VM stop/resize/restart")
            if runner.ingress:
                self.assertEqual(https_page(), persisted)
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
            print("REAL GAP HTTP + APPROVAL + KVM + COMPOSE BUILD + HTTP + PERSISTENCE + RESIZE + DELETE: OK", flush=True)
        finally:
            meta = manager.read(project, owner)
            if meta and meta['state'] != 'destroyed':
                if manager.alive(meta):
                    manager.perform(project, owner, 'vm/stop', {'vm_id': meta['vm_id'], 'force': True})
                manager.perform(project, owner, 'vm/destroy', {'vm_id': meta['vm_id'], 'delete_data': True, 'confirm_data_loss': True})


if __name__ == "__main__":
    unittest.main()
