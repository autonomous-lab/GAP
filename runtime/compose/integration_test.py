"""Real GAP HTTP -> runner HTTP -> GAP authorization, with a simulated guest.

Run after cargo build: GAP_TEST_BINARY=target/debug/gap python3 .../integration_test.py
This does not claim to boot a VM or execute Docker.
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
                config_path = root / "runner.json"
                config_path.write_text(json.dumps(config))
                executions = []
                def execute(vm, payload):
                    executions.append(payload)
                    return {"ok": True, "output": "simulated guest"}
                runner = Runner(config_path, execute=execute)
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
                if server:
                    server.shutdown()
                    server.server_close()
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


if __name__ == "__main__":
    unittest.main()
