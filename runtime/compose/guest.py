"""Install inside an exclusive project VM, never on the GAT host.

Compose and its referenced files are intentionally trusted inside this guest.
The owner may obtain guest root; this helper is not a guest security boundary.
"""
import base64
import binascii
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import signal
import subprocess
import sys
import tempfile

ROOT = Path("/var/lib/gap-compose")
MAX_BODY = 5 * 1024 * 1024


def validate_release(body):
    if set(body) != {"request_id", "compose_file", "files"}:
        raise ValueError("expected request_id, compose_file and files")
    if not isinstance(body["request_id"], str) or not re.fullmatch(r"[0-9a-f]{32}", body["request_id"]):
        raise ValueError("invalid request_id")
    files = body["files"]
    if not isinstance(files, dict) or not files:
        raise ValueError("files must be a nonempty path -> base64 object")
    decoded = {}
    total = 0
    for name, encoded in files.items():
        if not isinstance(name, str) or not name or len(name) > 512 or "\\" in name or any(ord(c) < 32 for c in name):
            raise ValueError("invalid bundle path")
        path = PurePosixPath(name)
        if path.is_absolute() or any(part in ("", ".", "..") for part in name.split("/")):
            raise ValueError("bundle paths must be relative files")
        if not isinstance(encoded, str):
            raise ValueError("file must contain base64")
        try:
            content = base64.b64decode(encoded, validate=True)
        except (ValueError, binascii.Error):
            raise ValueError("invalid base64")
        total += len(content)
        if total > MAX_BODY:
            raise ValueError("bundle exceeds transport budget")
        decoded[name] = content
    if not isinstance(body["compose_file"], str) or body["compose_file"] not in decoded:
        raise ValueError("compose_file must be in the bundle")
    for name in decoded:
        if any(str(parent) in decoded for parent in PurePosixPath(name).parents):
            raise ValueError("file/directory collision")
    return decoded


def compose(release, filename, *arguments):
    command = ["docker", "compose", "--project-name", "gap", "--project-directory", str(release),
               "--file", str(release / filename), *arguments]
    # The environment belongs to the guest. In particular no controller bearer,
    # SSH agent or host environment is forwarded across the VM boundary.
    env = {"PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
           "HOME": "/root", "DOCKER_HOST": "unix:///var/run/docker.sock",
           "COMPOSE_ANSI": "never", "DOCKER_BUILDKIT": "1"}
    environment_path = Path('/etc/gap/runtime.json')
    if environment_path.exists():
        from environment import validate
        env.update(validate(json.loads(environment_path.read_text())))
    with tempfile.TemporaryFile() as output:
        process = subprocess.Popen(command, cwd=release, env=env, stdin=subprocess.DEVNULL,
                                   stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        timed_out = False
        try:
            process.wait(timeout=540)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        size = output.tell()
        output.seek(max(0, size - 262144))
        return {"ok": process.returncode == 0 and not timed_out, "exit_code": process.returncode,
                "timed_out": timed_out, "output": output.read().decode("utf-8", errors="replace"),
                "output_truncated": size > 262144}


def run(payload, root=ROOT, execute=compose):
    action, body = payload["action"], payload["body"]
    if action == 'runtime_environment' and set(body) == {'variables'}:
        from environment import install
        install(body['variables'], ssh_dir=Path('/root/.ssh'))
        return {'ok': True}
    if action == 'ssh_keys' and set(body) == {'content'}:
        content = body['content']
        if not isinstance(content, str) or len(content) > 32768:
            raise ValueError('invalid keys')
        folder = Path('/root/.ssh')
        folder.mkdir(mode=0o700, exist_ok=True)
        with tempfile.NamedTemporaryFile(mode='w', dir=folder, delete=False) as out:
            out.write(content)
            out.flush()
            os.fsync(out.fileno())
        os.replace(out.name, folder / 'authorized_keys')
        return {'ok': True}
    if action == 'vm_probe' and body == {}:
        result = subprocess.run(['docker', 'info', '--format', '{{.ServerVersion}}'],
                                capture_output=True, timeout=10)
        return {'ok': result.returncode == 0, 'docker_version': result.stdout.decode().strip()}
    if action == 'vm_shutdown' and body == {}:
        # Guest-only fixed command. Reply before stopping SSH and the guest.
        subprocess.Popen(['/bin/sh', '-c', 'sleep 1; sync; /sbin/poweroff'],
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, start_new_session=True)
        return {'ok': True, 'shutdown_requested': True}
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (root / "operation.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return {"ok": False, "error": "guest_operation_in_progress"}
        if action == "releases":
            decoded = validate_release(body)
            release = root / "releases" / body["request_id"]
            digest = hashlib.sha256(json.dumps(body, sort_keys=True).encode()).hexdigest()
            metadata = root / (body["request_id"] + ".json")
            if metadata.exists():
                previous = json.loads(metadata.read_text())
                if previous["digest"] != digest:
                    return {"ok": False, "error": "immutable_release_conflict"}
                # Do not unknowingly replay a migration after a lost response.
                return {"ok": False, "error": "release_already_attempted_inspect_status"}
            release.mkdir(parents=True, exist_ok=False, mode=0o700)
            for name, content in decoded.items():
                target = release / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(content)
            metadata.write_text(json.dumps({"digest": digest, "compose_file": body["compose_file"]}))
            result = execute(release, body["compose_file"], "config", "--quiet")
            if result["ok"]:
                # Record attempted release before mutation: a partial `up` is
                # still the stack that stop/status must inspect.
                temp = root / "current.tmp"
                temp.write_text(json.dumps({"release": body["request_id"], "compose_file": body["compose_file"]}))
                temp.replace(root / "current.json")
                result = execute(release, body["compose_file"], "up", "--detach", "--build",
                                 "--remove-orphans", "--wait", "--wait-timeout", "120")
            result["release"] = body["request_id"]
            return result
        options = {"start": ("start",), "stop": ("stop",),
                   "status": ("ps", "--all", "--format", "json"),
                   "logs": ("logs", "--no-color", "--tail", "200")}
        if action not in options or set(body) != {"request_id"}:
            raise ValueError("invalid operation")
        if not (root / "current.json").exists():
            return {"ok": False, "error": "no_release"}
        current = json.loads((root / "current.json").read_text())
        return execute(root / "releases" / current["release"], current["compose_file"], *options[action])


if __name__ == "__main__":
    os.umask(0o077)
    try:
        raw = sys.stdin.buffer.read(MAX_BODY + 1)
        if len(raw) > MAX_BODY:
            raise ValueError("request too large")
        result = run(json.loads(raw))
    except Exception:
        result = {"ok": False, "error": "guest_operation_failed_inspect_status"}
    print(json.dumps(result, ensure_ascii=False))
