"""Preapproved Compose control worker. Runs QEMU and SSH, never Docker on the host.

Managed QEMU/KVM microVMs or legacy operator-provisioned guests are supported.
"""
import argparse
from contextlib import contextmanager, nullcontext, ExitStack
import hashlib
import hmac
import ipaddress
import json
import os
from pathlib import Path
import re
import selectors
import sqlite3
import subprocess
import tempfile
import threading
import time
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MAX_BODY = 5 * 1024 * 1024  # HTTP framing budget, not a guest storage quota
MAX_OUTPUT = 1024 * 1024
PROJECT = re.compile(r"prj_[0-9a-f]{24}\Z")
REQUEST = re.compile(r"[0-9a-f]{32}\Z")
ACTION = {"releases", "start", "stop", "status", "logs",
          "vm/create", "vm/start", "vm/stop", "vm/update", "vm/destroy", "vm/hibernate", "vm/resume", "runtime", "ingress", "ports", "ssh"}


class Failure(Exception):
    def __init__(self, status, code):
        self.status, self.code = status, code
        super().__init__(code)


def load_config(path):
    config = json.loads(Path(path).read_text())
    if config.get("approved_only") is not True:
        raise ValueError("runner requires explicit approved_only=true")
    seen = set()
    if config.get('hypervisor') and config.get('guests'):
        raise ValueError('choose managed hypervisor or legacy guest inventory, not both')
    for project, guest in config.get("guests", {}).items():
        if not PROJECT.fullmatch(project) or guest.get("microvm") is not True:
            raise ValueError("each project requires an operator-provisioned microVM")
        if not re.fullmatch(r"[a-zA-Z0-9_-]{1,64}", guest["vm_id"]):
            raise ValueError("invalid vm_id")
        ipaddress.ip_address(guest["address"])
        if type(guest["port"]) is not int or not 1 <= guest["port"] <= 65535:
            raise ValueError("invalid SSH port")
        if not re.fullmatch(r"did:gap:[0-9a-f]{64}", guest["owner_did"]):
            raise ValueError("invalid owner identity")
        if type(guest["expires_at"]) is not int:
            raise ValueError("approval expiry must be a Unix timestamp")
        # Prevent accidental reuse of one guest or credentials across projects.
        for item in (("vm", guest["vm_id"]), ("endpoint", guest["address"], guest["port"]),
                     ("key", str(Path(guest["ssh_key"]).resolve()))):
            if item in seen:
                raise ValueError("microVMs and SSH keys must be exclusive per project")
            seen.add(item)
        for key in ("ssh_key", "known_hosts"):
            if not Path(guest[key]).is_absolute():
                raise ValueError("operator SSH paths must be absolute")
    if not config["node_url"].startswith(("http://", "https://")):
        raise ValueError("invalid node_url")
    return config


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def ssh_command(guest):
    return ["ssh", "-F", "/dev/null", "-T", "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=yes", "-o", "IdentitiesOnly=yes",
            "-o", "IdentityAgent=none", "-o", "ForwardAgent=no",
            "-o", "ForwardX11=no", "-o", "ClearAllForwardings=yes",
            "-o", "PermitLocalCommand=no", "-o", "ConnectTimeout=10",
            "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=3",
            "-o", "GlobalKnownHostsFile=/dev/null",
            "-o", "UserKnownHostsFile=" + guest["known_hosts"],
            "-o", "HostKeyAlias=" + guest["vm_id"],
            "-i", guest["ssh_key"], "-p", str(guest["port"]),
            "root@" + guest["address"], "python3 /usr/local/lib/gap-compose-guest.py"]


def execute_guest(guest, payload, timeout=600):
    # Source is stdin, never part of a host command. No local Compose parsing,
    # env interpolation, archive extraction, Docker socket or host bind mounts.
    with tempfile.TemporaryFile() as source:
        source.write(json.dumps(payload).encode())
        source.seek(0)
        process = subprocess.Popen(ssh_command(guest), stdin=source, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT,
                                   env={"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"})
        output = bytearray()
        deadline = time.monotonic() + timeout
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                while True:
                    if time.monotonic() >= deadline:
                        raise Failure(504, "guest_timeout_state_unknown")
                    if not selector.select(1):
                        continue
                    chunk = os.read(process.stdout.fileno(), 65536)
                    if not chunk:
                        break
                    output.extend(chunk)
                    if len(output) > MAX_OUTPUT:
                        raise Failure(502, "guest_output_too_large_state_unknown")
            if process.wait(timeout=max(1, deadline - time.monotonic())) != 0:
                raise Failure(502, "guest_unreachable_or_failed_state_unknown")
            try:
                result = json.loads(output)
                if not isinstance(result, dict):
                    raise ValueError()
                return result
            except (ValueError, UnicodeError):
                raise Failure(502, "invalid_guest_response_state_unknown")
        finally:
            if process.poll() is None:
                process.kill()
            process.wait()
            process.stdout.close()


class Runner:
    def __init__(self, path, execute=execute_guest):
        self.path, self.execute = path, execute
        config = load_config(path)
        self.token = Path(config["token_file"]).read_text().strip()
        if len(self.token) < 32:
            raise ValueError("runner token must contain at least 32 characters")
        root = Path(config["state_dir"])
        root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.db_path = root / "jobs.sqlite"
        self.lock = threading.Lock()
        self.hypervisor = None
        self.hypervisor_config = config.get('hypervisor')
        if config.get('hypervisor'):
            from microvm import MicroVMs
            self.hypervisor = MicroVMs(config['hypervisor'], execute)
            self.hypervisor.quota_provider = lambda project, owner: self.authorize(project, owner)['quota']
        self.runtime = None
        self.operator_token = None
        if config.get('serverless'):
            if not self.hypervisor: raise ValueError('serverless_requires_managed_microvms')
            self.operator_token=Path(config['operator_token_file']).read_text().strip()
            if len(self.operator_token)<32 or self.operator_token==self.token: raise ValueError('distinct_operator_token_required')
            from lifecycle import Runtime
            from gateway import Gateway
            self.runtime=Runtime(self,config)
            Gateway(self.runtime,config.get('wake_gateway_port',8094))
        self.ingress = None
        self.ingress_config = config.get('ingress')
        if self.ingress_config:
            from ingress import Ingress
            self.ingress = Ingress(self.ingress_config, self.hypervisor)
        with self.db() as db:
            db.execute("""CREATE TABLE IF NOT EXISTS jobs(
                id TEXT PRIMARY KEY, project TEXT NOT NULL, owner TEXT NOT NULL,
                request TEXT NOT NULL, digest TEXT NOT NULL, action TEXT NOT NULL,
                payload TEXT, status TEXT NOT NULL, result TEXT, created INTEGER NOT NULL,
                UNIQUE(project, request))""")
            # Never replay potentially executed remote mutations automatically.
            db.execute("UPDATE jobs SET status='interrupted', payload=NULL WHERE status IN ('queued','running')")
        os.chmod(self.db_path, 0o600)
        if self.runtime:
            threading.Thread(target=self.runtime.run,daemon=True).start()

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.db_path, timeout=5)
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    def authorize(self, project, owner):
        config = load_config(self.path)  # reload approvals before each operation
        if config.get('ingress') != self.ingress_config:
            raise Failure(503, 'ingress_config_changed_restart_worker')
        guest = config.get("guests", {}).get(project)
        if self.hypervisor:
            if config.get('hypervisor') != self.hypervisor_config:
                raise Failure(503, 'hypervisor_config_changed_restart_worker')
            guest = {'managed': True, 'project_id': project, 'owner_did': owner}
        elif not guest or guest["owner_did"] != owner or guest["expires_at"] <= time.time():
            raise Failure(403, "compose_not_preapproved")
        request = urllib.request.Request(config["node_url"].rstrip("/") + "/internal/compose/authorize",
            data=json.dumps({"project_id": project, "owner_did": owner}).encode(),
            headers={"Authorization": "Bearer " + self.token, "Content-Type": "application/json"})
        try:
            with urllib.request.build_opener(NoRedirect).open(request, timeout=5) as response:
                approval = json.loads(response.read(4096))
                if response.status != 200 or approval.get("allowed") is not True:
                    raise ValueError()
        except Exception:
            raise Failure(403, "compose_approval_unavailable_or_revoked")
        if self.hypervisor:
            quota = approval.get("quota")
            if (not isinstance(quota, dict) or not {"vcpus", "memory_mib"} <= set(quota) <= {"vcpus", "memory_mib", "max_vms"}
                    or any(type(v) is not int or not 0 < v < 2**31 for v in quota.values())):
                raise Failure(403, "microvm_quota_unavailable")
            guest["quota"] = quota
            guest["always_on_allowed"] = approval.get("always_on_allowed") is True
        return guest

    @staticmethod
    def public_job(row):
        return {"job_id": row["id"], "request_id": row["request"], "action": row["action"],
                "status": row["status"], "created_at": row["created"],
                "result": json.loads(row["result"]) if row["result"] else None}

    def rpc(self, rpc):
        # Internal service credential required by /rpc. The public project
        # route allowlist never forwards this operator-only inventory action.
        if rpc.get('action')=='admin/inventory' and rpc.get('method')=='GET':
            if not self.hypervisor:return 200,{'vms':[],'available':False}
            body=rpc.get('body') or {};offset=body.get('offset',0)
            if type(offset) is not int or not 0<=offset<=1000000:raise Failure(400,'invalid_offset')
            paths=sorted((self.hypervisor.root/'catalog').glob('*.json'))
            values=[]
            for path in paths[offset:offset+100]:
                meta=json.loads(path.read_text())
                view=self.hypervisor.public(meta)
                view['owner_did']=meta['owner_did']
                if self.runtime:view['runtime']=self.runtime.view(meta,include_entries=False)
                if self.ingress:view['ingress']=self.ingress.public(meta)
                values.append(view)
            return 200,{'vms':values,'total':len(paths),'offset':offset,'limit':100,'available':True}
        if rpc.get('action')=='admin/finance' and rpc.get('method')=='GET':
            if not self.runtime:return 200,{'available':False}
            body=rpc.get('body') or {}
            return 200,dict(self.runtime.ledger.finance_report(body['start'],body['end'],body.get('project_id')),available=True)
        project, owner = rpc.get("project_id", ""), rpc.get("owner_did", "")
        if not isinstance(project, str) or not PROJECT.fullmatch(project) or not isinstance(owner, str):
            raise Failure(400, "invalid_project")
        self.authorize(project, owner)
        method, action, body = rpc.get("method"), rpc.get("action"), rpc.get("body")
        selected = body.get('vm_id') if isinstance(body,dict) else None
        if selected is not None and not re.fullmatch(r'vm_[0-9a-f]{32}',str(selected)):
            raise Failure(400,'invalid_vm_identity')
        if action=='vms':
            if not self.hypervisor: raise Failure(409,'managed_hypervisor_not_configured')
            if method=='GET':
                return 200,{'vms':[self.hypervisor.public(m) for m in self.hypervisor.list(project,owner) if m['state']!='destroyed'],
                            'agent_quota':self.hypervisor.quota_view(project,owner)}
            if method!='POST' or not isinstance(body,dict): raise Failure(400,'invalid_vm_collection_method')
            body=dict(body,new_vm=True)
            action='vm'
        if action in ('runtime','credits','budget'):
            if not self.runtime: raise Failure(409,'serverless_not_configured')
            if method=='GET':
                if action=='runtime': return 200,self.runtime.view(self.hypervisor.read(project,owner,selected))
                return 200,self.runtime.ledger.view(project,owner)
            if action=='budget' and method=='PUT':
                if not isinstance(body,dict) or set(body)!={'request_id','budget_microcredits'}:
                    raise Failure(400,'invalid_budget')
                with self.runtime.lock(project):
                    for meta in self.hypervisor.list(project,owner): self.runtime.sample(meta)
                    return 200,self.runtime.ledger.set_budget(project,owner,body['budget_microcredits'],body['request_id'])
            if action!='runtime' or method!='PUT': raise Failure(400,'invalid_runtime_method')
            method='POST'
        if action in ('ports', 'ssh'):
            if not self.hypervisor or not self.hypervisor.network:
                raise Failure(409, 'public_network_not_configured')
            if method == 'GET':
                meta = self.hypervisor.read(project, owner,selected)
                view = self.hypervisor.network.public if action == 'ports' else self.hypervisor.network.ssh_public
                return 200, view(meta)
            if method != 'PUT':
                raise Failure(400, 'invalid_network_method')
            method = 'POST'
        if action == 'ingress':
            if not self.ingress:
                raise Failure(409, 'ingress_not_configured')
            if method == 'GET':
                return 200, self.ingress.public(self.hypervisor.read(project, owner,selected))
            if method != 'PUT':
                raise Failure(400, 'invalid_ingress_method')
            method = 'POST'
        if action == 'vm':
            if method == 'GET':
                if not self.hypervisor:
                    raise Failure(409, 'managed_hypervisor_not_configured')
                view = self.hypervisor.public(self.hypervisor.read(project, owner,selected))
                view["agent_quota"] = self.hypervisor.quota_view(project, owner)
                if self.runtime: view["runtime"] = self.runtime.view(self.hypervisor.read(project,owner,selected))
                return 200, view
            action = {'POST': 'vm/create', 'PATCH': 'vm/update', 'DELETE': 'vm/destroy'}.get(method)
            method = 'POST'
        if method == "GET" and isinstance(action, str):
            with self.db() as db:
                if action == "":
                    row = db.execute("SELECT * FROM jobs WHERE project=? AND owner=? ORDER BY created DESC,rowid DESC LIMIT 1", (project, owner)).fetchone()
                    return 200, {"project_id": project, "latest_job": self.public_job(row) if row else None,
                                 "note": "job history, not live VM or application health"}
                if re.fullmatch(r"jobs/job_[0-9a-f]{32}", action):
                    row = db.execute("SELECT * FROM jobs WHERE project=? AND owner=? AND id=?", (project, owner, action[5:])).fetchone()
                    if row:
                        return 200, self.public_job(row)
            raise Failure(404, "unknown_stack_resource")
        if method != "POST" or not isinstance(action, str) or action not in ACTION or not isinstance(body, dict):
            raise Failure(400, "invalid_stack_operation")
        request_id = body.get("request_id")
        if not isinstance(request_id, str) or not REQUEST.fullmatch(request_id):
            raise Failure(400, "request_id_must_be_32_lowercase_hex_characters")
        if action.startswith('vm/'):
            if not self.hypervisor:
                raise Failure(409, 'managed_hypervisor_not_configured')
            from microvm import validate, VMError
            try:
                validate(action, body)
            except VMError as error:
                raise Failure(400, str(error))
        elif action in ('ports', 'ssh'):
            from network import validate
            from microvm import VMError
            try:
                validate(action, body)
            except VMError as error:
                raise Failure(400, str(error))
        elif action == 'ingress':
            from microvm import VMError
            try:
                self.ingress.validate(body)
            except VMError as error:
                raise Failure(400, str(error))
        elif action == "runtime":
            if not {"request_id","vm_id","mode"} <= set(body) <= {"request_id","vm_id","mode","idle_timeout_seconds"}: raise Failure(400,"invalid_runtime_fields")
        elif action == "releases":
            from guest import validate_release
            try:
                validate_release(body)
            except ValueError:
                raise Failure(400, "invalid_release_bundle")
        elif set(body) != {"request_id"}:
            raise Failure(400, "unexpected_operation_fields")
        payload = json.dumps({"action": action, "body": body}, sort_keys=True, separators=(",", ":"))
        digest = hashlib.sha256(payload.encode()).hexdigest()
        with self.lock, self.db() as db:
            old = db.execute("SELECT * FROM jobs WHERE project=? AND request=?", (project, request_id)).fetchone()
            if old:
                if old["digest"] != digest or old["owner"] != owner:
                    raise Failure(409, "request_id_conflict")
                return 200, self.public_job(old)
            if db.execute("SELECT 1 FROM jobs WHERE project=? AND status IN ('queued','running')", (project,)).fetchone():
                raise Failure(409, "stack_operation_in_progress")
            job = "job_" + uuid.uuid4().hex
            db.execute("INSERT INTO jobs VALUES(?,?,?,?,?,?,?,'queued',NULL,?)",
                       (job, project, owner, request_id, digest, action, payload, int(time.time())))
        threading.Thread(target=self.run_job, args=(job,), daemon=True).start()
        return 202, {"job_id": job, "request_id": request_id, "status": "queued"}

    def run_job(self, job):
        try:
            with self.db() as db:
                row = db.execute("SELECT * FROM jobs WHERE id=?", (job,)).fetchone()
            guest = self.authorize(row["project"], row["owner"])
            with self.db() as db:
                db.execute("UPDATE jobs SET status='running' WHERE id=?", (job,))
            payload = json.loads(row['payload'])
            with self.runtime.lock(row['project']) if self.runtime else nullcontext():
                if self.runtime:
                    current=self.hypervisor.read(row['project'],row['owner'],payload['body'].get('vm_id'))
                    if current: self.runtime.sample(current)
                    if payload['action'] in ('vm/create','vm/start','vm/resume'):
                        self.runtime.check_credit({'project_id':row['project'],'owner_did':row['owner']})
                    if current and current['state']=='hibernated' and payload['action'] not in ('vm/create','vm/start','vm/resume','vm/stop','vm/hibernate','vm/destroy','vm/update','runtime'):
                        self.runtime.ensure_awake(row['project'],current['vm_id'])
                admission=nullcontext()
                if self.runtime and payload['action'] in ('vm/create','vm/start','vm/resume'):
                    resources=payload['body'] if payload['action']=='vm/create' else current
                    admission=self.runtime.admission(row['project'],resources.get('vcpus',1),resources.get('memory_mib',1024),payload['body'].get('vm_id'))
                with admission: result=self.dispatch_job(row,guest,payload)
                if self.runtime:
                    current=self.hypervisor.read(row['project'],row['owner'],result.get('vm',{}).get('vm_id') or payload['body'].get('vm_id'))
                    if current:
                        if payload['action'] in ('vm/stop','vm/hibernate'): current['manual_stop']=True
                        elif payload['action'] in ('vm/start','vm/resume','vm/create'):
                            current['manual_stop']=False; current.pop('runtime_error',None)
                        self.hypervisor.save(current)
                        self.runtime.sample(current)
                    self.runtime.gateway.reconcile()
                    if self.ingress: self.ingress.sync()
            status = "succeeded" if result.get("ok") is True else "failed"
        except Failure as error:
            status, result = "failed", {"error": error.code}
        except Exception as error:
            from microvm import VMError
            from billing import BillingError
            status, result = "failed", {"error": str(error) if isinstance(error, (VMError,BillingError)) else "runner_failed_state_unknown"}
        with self.db() as db:
            db.execute("UPDATE jobs SET status=?,result=?,payload=NULL WHERE id=?", (status, json.dumps(result), job))

    def dispatch_job(self,row,guest,payload):
        if payload['action']=='runtime':
            return self.runtime.configure(row['project'],row['owner'],payload['body'])
        if payload['action'].startswith('vm/'):
            operation = self.ingress.vm_operation if self.ingress else self.hypervisor.perform
            result = operation(row['project'], row['owner'], payload['action'], payload['body'])
        elif payload['action'] in ('ports', 'ssh'):
            result = self.hypervisor.network.perform(row['project'], row['owner'], payload['action'], payload['body'])
        elif payload['action'] == 'ingress':
            result = self.ingress.perform(row['project'], row['owner'], payload['body'])
        else:
            if self.hypervisor:
                guest = self.hypervisor.guest(row['project'], row['owner'])
                meta = self.hypervisor.read(row['project'], row['owner'])
                self.hypervisor.sync_environment(meta)
            result = self.execute(guest, payload)
        return result

    def operator(self,body):
        if not self.runtime: raise Failure(409,'serverless_not_configured')
        ledger=self.runtime.ledger
        action=body.get('action')
        if action=='pricing': return ledger.pricing()
        if action=='finance':return ledger.finance_report(body['start'],body['end'],body.get('project_id'))
        if action=='set-costs':return ledger.finance_costs(body['costs'],body['expected_version'])
        if action=='classify-funding':return ledger.classify_funding(body['project_id'],body['operation'],body['source'],body['cash_microdollars'],body['note'])
        if action=='set-pricing':
            # Flush all current allocation intervals before changing the price.
            metas=[json.loads(p.read_text()) for p in sorted((self.hypervisor.root/'catalog').glob('*.json'))]
            with ExitStack() as locks:
                for meta in metas: locks.enter_context(self.runtime.lock(meta['project_id']))
                for meta in metas: self.runtime.sample(self.hypervisor.read(meta['project_id'],meta['owner_did'],meta['vm_id']))
                return ledger.set_pricing(body['mode'],body.get('tariff'),body.get('expected_version',...))
        if action=='topup':
            self.authorize(body['project_id'],body['owner_did'])
            with self.runtime.lock(body['project_id']):
                for meta in self.hypervisor.list(body['project_id'],body['owner_did']): self.runtime.sample(meta)
                return ledger.topup(body['project_id'],body['owner_did'],body['amount_microcredits'],body['request_id'])
        if action=='account': return ledger.view(body['project_id'],body['owner_did'])
        raise Failure(400,'invalid_operator_action')


def handler_for(runner):
    from billing import BillingError
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass  # Never put manifests, bearer tokens or guest logs in host logs.

        def reply(self, status, value):
            body = json.dumps(value).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            self.connection.settimeout(10)
            try:
                if self.path not in ("/rpc","/operator"):
                    raise Failure(404, "unknown_route")
                supplied = self.headers.get("Authorization", "").encode()
                secret=runner.operator_token if self.path=="/operator" else runner.token
                if not secret or not hmac.compare_digest(supplied, ("Bearer " + secret).encode()):
                    raise Failure(401, "unauthorized")
                if self.headers.get("Transfer-Encoding") or len(self.headers.get_all("Content-Length", [])) != 1:
                    raise Failure(400, "content_length_required")
                length = int(self.headers["Content-Length"])
                if not 0 < length <= MAX_BODY:
                    raise Failure(413, "request_too_large")
                raw = self.rfile.read(length)
                if len(raw) != length:
                    raise Failure(400, "incomplete_body")
                rpc = json.loads(raw)
                if not isinstance(rpc, dict):
                    raise Failure(400, "invalid_request")
                status, value = (200,runner.operator(rpc)) if self.path=="/operator" else runner.rpc(rpc)
            except Failure as error:
                status, value = error.status, {"error": {"code": error.code}}
            except BillingError as error:
                code=str(error)
                status=409 if code in ('immutable_tariff_version','tariff_version_changed_refresh_before_retry','billing_operation_conflict','immutable_cost_version','cost_version_changed_refresh_before_retry','immutable_funding_classification') else 400
                value={"error":{"code":code}}
            except (ValueError, UnicodeError, TimeoutError, RecursionError):
                status, value = 400, {"error": {"code": "invalid_request"}}
            except Exception:
                status, value = 503, {"error": {"code": "runner_unavailable"}}
            self.reply(status, value)
    return Handler


if __name__ == "__main__":
    os.umask(0o077)
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8092)
    args = parser.parse_args()
    if ipaddress.ip_address(args.bind).is_unspecified and load_config(args.config).get('listen_all') is not True:
        raise SystemExit("bind to an explicit loopback/private interface, not all interfaces")
    # A single worker process must own the job journal.
    import fcntl
    state_dir = Path(load_config(args.config)["state_dir"])
    state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (state_dir / "runner.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        runner = Runner(args.config)
        ThreadingHTTPServer((args.bind, args.port), handler_for(runner)).serve_forever()
