import base64
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

import guest
from runner import Runner, Failure, load_config, ssh_command

PROJECT = "prj_" + "a" * 24
OTHER = "prj_" + "b" * 24
OWNER = "did:gap:" + "c" * 64


def release(request="a" * 32):
    return {"request_id": request, "compose_file": "compose.yaml", "files": {
        "compose.yaml": base64.b64encode(b"services:\n  app:\n    image: alpine\n    privileged: true\n").decode(),
        ".env": base64.b64encode(b"PROJECT_ENV=guest-only").decode()}}


class BundleTests(unittest.TestCase):
    def test_classic_compose_and_env_are_guest_data(self):
        files = guest.validate_release(release())
        self.assertIn(b"privileged: true", files["compose.yaml"])
        self.assertIn(".env", files)

    def test_path_and_base64_validation(self):
        for name in ("/etc/passwd", "../x", "x/../../x", "x//y", "./x", "x\\y", "x\x00y"):
            body = release()
            body["files"][name] = "eA=="
            with self.assertRaises(ValueError, msg=name):
                guest.validate_release(body)
        body = release()
        body["files"]["compose.yaml"] = "not base64!"
        with self.assertRaises(ValueError):
            guest.validate_release(body)
        body = release()
        body["files"]["compose.yaml/subfile"] = "eA=="
        with self.assertRaises(ValueError):
            guest.validate_release(body)

    def test_guest_lifecycle_no_host_interpolation_or_blind_replay(self):
        calls = []
        def execute(path, filename, *args):
            calls.append((path, filename, args))
            return {"ok": True}
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            result = guest.run({"action": "releases", "body": release()}, root, execute)
            self.assertTrue(result["ok"])
            self.assertEqual(calls[0][2], ("config", "--quiet"))
            self.assertEqual(calls[1][2][0], "up")
            self.assertEqual((calls[0][0] / ".env").read_text(), "PROJECT_ENV=guest-only")
            retry = guest.run({"action": "releases", "body": release()}, root, execute)
            self.assertFalse(retry["ok"])
            self.assertEqual(len(calls), 2)
            for action in ("status", "logs", "stop", "start"):
                self.assertTrue(guest.run({"action": action, "body": {"request_id": "b" * 32}}, root, execute)["ok"])
            self.assertEqual(calls[-2][2], ("stop",))
            self.assertTrue((root / "releases" / ("a" * 32)).exists())

    def test_partial_up_remains_inspectable(self):
        def execute(path, filename, *args):
            return {"ok": args[0] != "up"}
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            result = guest.run({"action": "releases", "body": release()}, root, execute)
            self.assertFalse(result["ok"])
            self.assertTrue((root / "current.json").exists())
            self.assertTrue(guest.run({"action": "stop", "body": {"request_id": "b" * 32}}, root, execute)["ok"])


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        (root / "token").write_text("t" * 40)
        self.path = root / "config.json"
        self.config = {"private_node": True, "token_file": str(root / "token"),
                       "state_dir": str(root / "state"), "node_url": "http://127.0.0.1:8080",
                       "guests": {PROJECT: {"microvm": True, "vm_id": "project-a",
                           "owner_did": OWNER, "expires_at": int(time.time()) + 3600,
                           "address": "127.0.0.1", "port": 22001,
                           "ssh_key": str(root / "key"), "known_hosts": str(root / "known_hosts")}}}
        self.save()

    def save(self):
        self.path.write_text(json.dumps(self.config))

    def test_config_requires_private_exclusive_microvms(self):
        for key, value in (("private_node", False),):
            self.config[key] = value
            self.save()
            with self.assertRaises(ValueError):
                load_config(self.path)
        self.config["private_node"] = True
        self.config["guests"][OTHER] = dict(self.config["guests"][PROJECT])
        self.save()
        with self.assertRaises(ValueError):
            load_config(self.path)

    def test_ssh_is_fixed_and_does_not_forward_control_credentials(self):
        cmd = ssh_command(self.config["guests"][PROJECT])
        self.assertEqual(cmd[0], "ssh")
        self.assertEqual(cmd[-1], "python3 /usr/local/lib/gap-compose-guest.py")
        for option in ("ForwardAgent=no", "StrictHostKeyChecking=yes", "IdentityAgent=none", "ClearAllForwardings=yes"):
            self.assertIn(option, cmd)
        self.assertNotIn("docker", cmd)

    def test_missing_wrong_and_expired_approval_deny_before_network(self):
        runner = Runner(self.path)
        for project, owner in ((OTHER, OWNER), (PROJECT, "did:gap:" + "d" * 64)):
            with self.assertRaises(Failure) as failure:
                runner.authorize(project, owner)
            self.assertEqual(failure.exception.code, "compose_not_preapproved")
        self.config["guests"][PROJECT]["expires_at"] = 1
        self.save()
        with self.assertRaises(Failure):
            runner.authorize(PROJECT, OWNER)

    def test_node_revalidation_fails_closed(self):
        runner = Runner(self.path)
        with patch("urllib.request.build_opener") as opener:
            opener.return_value.open.side_effect = OSError("node unavailable")
            with self.assertRaises(Failure) as failure:
                runner.authorize(PROJECT, OWNER)
            self.assertEqual(failure.exception.code, "compose_approval_unavailable_or_revoked")

    def test_idempotency_busy_jobs_scoping_and_payload_erasure(self):
        gate, started = threading.Event(), threading.Event()
        executions = []
        def execute(vm, payload):
            executions.append(payload)
            started.set()
            gate.wait(3)
            return {"ok": True}
        runner = Runner(self.path, execute=execute)
        self.addCleanup(gate.set)
        def authorize(project, owner):
            if project != PROJECT or owner != OWNER:
                raise Failure(403, "compose_not_preapproved")
            return self.config["guests"][project]
        runner.authorize = authorize
        rpc = {"project_id": PROJECT, "owner_did": OWNER, "method": "POST", "action": "releases", "body": release()}
        status, result = runner.rpc(rpc)
        self.assertEqual(status, 202)
        self.assertTrue(started.wait(1))
        self.assertEqual(runner.rpc(rpc)[1]["job_id"], result["job_id"])
        changed = {**rpc, "body": {**release(), "files": {"compose.yaml": "eA=="}}}
        with self.assertRaises(Failure) as conflict:
            runner.rpc(changed)
        self.assertEqual(conflict.exception.status, 409)
        with self.assertRaises(Failure) as busy:
            runner.rpc({**rpc, "body": release("b" * 32)})
        self.assertEqual(busy.exception.code, "stack_operation_in_progress")
        with self.assertRaises(Failure):
            runner.rpc({**rpc, "project_id": OTHER})
        gate.set()
        poll = {**rpc, "method": "GET", "action": "jobs/" + result["job_id"]}
        for _ in range(100):
            value = runner.rpc(poll)[1]
            if value["status"] == "succeeded":
                break
            time.sleep(.01)
        self.assertEqual(value["status"], "succeeded")
        self.assertEqual(len(executions), 1)
        with runner.db() as db:
            self.assertIsNone(db.execute("SELECT payload FROM jobs").fetchone()[0])

    def test_revocation_before_execution(self):
        executions = []
        runner = Runner(self.path, execute=lambda *args: executions.append(args))
        count = 0
        def authorize(*args):
            nonlocal count
            count += 1
            if count > 1:
                raise Failure(403, "revoked")
            return self.config["guests"][PROJECT]
        runner.authorize = authorize
        _, value = runner.rpc({"project_id": PROJECT, "owner_did": OWNER, "method": "POST",
                               "action": "releases", "body": release()})
        for _ in range(100):
            with runner.db() as db:
                row = db.execute("SELECT status FROM jobs WHERE id=?", (value["job_id"],)).fetchone()
            if row[0] == "failed":
                break
            time.sleep(.01)
        self.assertEqual(row[0], "failed")
        self.assertFalse(executions)


if __name__ == "__main__":
    unittest.main()
