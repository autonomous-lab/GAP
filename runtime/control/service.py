"""Private HTTP transport for the operator account authority.

Bind behind the operator's trusted TLS edge or an authenticated tunnel. Never
expose the cleartext listener to the Internet. Nodes have individual credentials;
neither node credentials nor client tokens grant operator administration.
"""
import argparse
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
import os
from pathlib import Path
import re
import sqlite3
import threading
from urllib.parse import urlsplit, parse_qs

from authority import Authority, Failure, identifier


def secret(path):
    value = Path(path).read_text().strip()
    if not re.fullmatch(r'[A-Za-z0-9_-]{43,128}', value):
        raise ValueError('invalid control credential file')
    return value


class Application:
    def __init__(self, authority, admin, nodes, allow_debits=False, allow_reservations=False, allow_capacity=False, access=None, identity_nodes=None):
        self.authority, self.admin, self.nodes = authority, admin, dict(nodes)
        self.allow_debits = allow_debits
        self.access, self.identity_nodes = access, dict(identity_nodes or {})
        if type(allow_reservations) is not bool:
            raise ValueError('invalid reservation configuration')
        self.allow_reservations = allow_reservations
        if type(allow_capacity) is not bool:
            raise ValueError('invalid capacity configuration')
        self.allow_capacity = allow_capacity
        credentials = [admin, *nodes.values(), *self.identity_nodes.values()]
        if type(allow_debits) is not bool or len(set(credentials)) != len(credentials):
            raise ValueError('control credentials must be distinct')
        if not set(self.identity_nodes).issubset(nodes):
            raise ValueError('unknown identity gateway')
        for node in nodes:
            identifier(node)

    def actor(self, token):
        if hmac.compare_digest(token, self.admin):
            return 'operator', None
        for node, credential in self.nodes.items():
            if hmac.compare_digest(token, credential):
                return 'node', node
        for node, credential in self.identity_nodes.items():
            if hmac.compare_digest(token, credential):
                return 'identity', node
        return 'client', self.authority.authenticate(token)

    def handle(self, method, path, token, body):
        parsed = urlsplit(path)
        if method == 'GET' and parsed.path == '/health':
            with self.authority.db() as db:
                row = db.execute("SELECT value FROM metadata WHERE key='operator'").fetchone()
                if not row or row[0] != self.authority.operator:
                    raise Failure('authority_unavailable', 503)
            return {'ok': True, 'operator_id': self.authority.operator, 'phase': 'reservations',
                    'legacy_cutover': False, 'worker_metering_mode': 'opt_in',
                    'reservation_protocol': 1, 'reservations_enabled': self.allow_reservations,
                    'capacity_protocol': 2, 'capacity_enabled': self.allow_capacity,
                    'worker_capacity_enforcement': 'explicit_project_opt_in',
                    'project_access_enabled': self.access is not None,
                    'online_debits_enabled': self.allow_debits}
        kind, actor = self.actor(token)
        a = self.authority
        if method == 'POST' and parsed.path == '/identity':
            if kind != 'identity':
                raise Failure('identity_gateway_credentials_required', 403)
            if not self.access:
                raise Failure('fleet_access_disabled', 409)
            if body.get('action') != 'connect':
                raise Failure('unknown_identity_action', 404)
            return self.access.connect(actor, body['request_id'], body['email'], body['agent_did'], body['project_id'])
        if method == 'POST' and parsed.path == '/operator':
            if kind != 'operator':
                raise Failure('operator_credentials_required', 403)
            action = body.get('action')
            request = body.get('request_id')
            if action == 'create-customer':
                return a.create_customer('operator', request, body['label'])
            if action == 'attach-principal':
                return a.attach_principal('operator', request, body['customer_id'], body['kind'], body['subject'], body.get('verified', False))
            if action == 'attach-project':
                if body['node_id'] not in self.nodes:
                    raise Failure('unknown_trusted_node')
                return a.attach_project('operator', request, body['customer_id'], body['project_id'], body['node_id'], body['owner_did'])
            if action == 'grant':
                return a.grant('operator', request, body['project_id'], body['agent_did'], body['role'])
            if action == 'topup':
                return a.topup('operator', request, body['customer_id'], body['amount_microcredits'], body['source'])
            if action == 'stage-import':
                return a.stage_import('operator', request, body['customer_id'], body['node_id'], body['project_id'], body['snapshot'])
            if action == 'wallet':
                return a.wallet(body['customer_id'])
            if action == 'quotas':
                return a.quotas(body['customer_id'])
            if action == 'set-quotas':
                return a.set_quotas('operator', request, body['customer_id'], body['limits'], body['expected_revision'])
            if action == 'capacity-list':
                return a.capacity_list(body['customer_id'], body.get('after', ''))
            if action == 'projects':
                return a.projects(body['customer_id'], after=body.get('after', ''))
            if action == 'issue-token':
                # Tokens are intentionally not stored in idempotency responses.
                # An uncertain issuance can be repeated; every token expires.
                return a.issue(body['customer_id'], body.get('agent_did'), body.get('ttl_seconds', 3600))
            raise Failure('unknown_operator_action', 404)
        if method == 'POST' and parsed.path == '/node':
            if kind != 'node':
                raise Failure('node_credentials_required', 403)
            if body.get('action') in ('capacity-get', 'capacity-prepare', 'capacity-finish', 'capacity-cancel-create', 'capacity-abort-resize'):
                if not self.allow_capacity:
                    raise Failure('capacity_disabled', 409)
                if body['action'] == 'capacity-get':
                    return a.capacity_get(actor, body['project_id'], body['vm_id'])
                if body['action'] == 'capacity-prepare':
                    return a.capacity_prepare(actor, body['request_id'], body['project_id'], body['owner_did'],
                        body['vm_id'], body['cpu_quarters'], body['memory_mib'], body['expected_revision'])
                if body['action'] == 'capacity-cancel-create':
                    return a.capacity_cancel_create(actor,body['request_id'],body['project_id'],body['owner_did'],body['vm_id'],body['evidence_id'])
                if body['action'] == 'capacity-abort-resize':
                    return a.capacity_abort_resize(actor,body['request_id'],body['project_id'],body['vm_id'],body['expected_revision'],body['transition_id'],body['evidence_id'])
                return a.capacity_finish(actor, body['request_id'], body['project_id'], body['vm_id'],
                    body['expected_revision'], body.get('transition_id'), body['outcome'], body['evidence_id'])
            if body.get('action') == 'project':
                with a.db() as db:
                    placement = dict(a.node_project(db, actor, body['project_id']))
                return {'operator_id': a.operator, **placement}
            if body.get('action') == 'debit':
                if not self.allow_debits:
                    raise Failure('online_debits_disabled', 409)
                return a.debit(actor, body['request_id'], body['project_id'], body['amount_microcredits'])
            if body.get('action') == 'checkpoint':
                if not self.allow_reservations:
                    raise Failure('reservations_disabled', 409)
                result = a.checkpoint(actor,body['request_id'],body['project_id'],body['owner_did'],
                    body['reservation_id'],body['consumed_microcredits'],body['unpaid_microcredits'],
                    body['target_microcredits'],body['lease_seconds'],body.get('close',False))
                return dict(result,authority_now=math.ceil(a.clock()))
            raise Failure('unknown_node_action', 404)
        if kind != 'client':
            raise Failure('client_credentials_required', 403)
        if method == 'GET' and parsed.path == '/v1/account':
            return {'operator_id': a.operator, 'customer_id': actor['customer'], 'agent_did': actor['agent']}
        if method == 'GET' and parsed.path == '/v1/wallet':
            return a.wallet(actor['customer'])
        if method == 'GET' and parsed.path == '/v1/quotas':
            # Aggregate only: no other agent's project/VM identifiers disclosed.
            return a.quotas(actor['customer'])
        if method == 'GET' and parsed.path == '/v1/projects':
            after = parse_qs(parsed.query).get('after', [''])[0]
            return a.projects(actor['customer'], actor['agent'], after)
        if method == 'POST' and parsed.path == '/v1/logout':
            return a.revoke(token)
        if method == 'POST' and parsed.path == '/v1/project-token':
            if not self.access:
                raise Failure('fleet_access_disabled', 409)
            return self.access.issue(actor, body['project_id'], body.get('ttl_seconds', 120))
        raise Failure('not_found', 404)


class Server(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, application):
        self.application = application
        self.slots = threading.BoundedSemaphore(32)
        super().__init__(address, Handler)

    def process_request(self, request, client_address):
        if not self.slots.acquire(blocking=False):
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except BaseException:
            self.slots.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self.slots.release()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass  # No request paths, payloads or credentials in logs.

    def setup(self):
        super().setup()
        self.connection.settimeout(10)

    def do_GET(self):
        self.respond()

    def do_POST(self):
        self.respond()

    def respond(self):
        status = 200
        try:
            if self.headers.get('Transfer-Encoding') or len(self.headers.get_all('Content-Length', [])) > 1:
                raise Failure('invalid_request_framing')
            length = int(self.headers.get('Content-Length', '0'))
            if not 0 <= length <= 65536:
                raise Failure('request_too_large', 413)
            raw = self.rfile.read(length)
            if len(raw) != length:
                raise Failure('incomplete_request')
            body = json.loads(raw, parse_constant=lambda _: (_ for _ in ()).throw(ValueError())) if raw else {}
            if not isinstance(body, dict):
                raise Failure('invalid_request')
            auth = self.headers.get_all('Authorization', [])
            if len(auth) > 1:
                raise Failure('invalid_control_credentials', 401)
            token = auth[0][7:] if auth and auth[0].startswith('Bearer ') else ''
            result = self.server.application.handle(self.command, self.path, token, body)
        except Failure as error:
            status, result = error.status, {'error': {'code': error.code}}
        except (KeyError, TypeError, ValueError, OverflowError):
            status, result = 400, {'error': {'code': 'invalid_request'}}
        except (sqlite3.Error, OSError):
            status, result = 503, {'error': {'code': 'authority_unavailable'}}
        encoded = json.dumps(result, allow_nan=False).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.send_header('Cache-Control', 'no-store')
        self.send_header('X-Content-Type-Options', 'nosniff')
        self.send_header('Connection', 'close')
        self.end_headers()
        self.wfile.write(encoded)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', required=True)
    parser.add_argument('--bind', default='127.0.0.1')
    parser.add_argument('--port', type=int, default=8096)
    args = parser.parse_args()
    os.umask(0o077)
    try:
        config = json.loads(Path(args.config).read_text())
        authority = Authority(config['database'], config['operator_id'])
        access = None
        if config.get('signing_seed_file'):
            from access import Access
            access = Access(authority, bytes.fromhex(Path(config['signing_seed_file']).read_text().strip()))
        app = Application(authority, secret(config['operator_token_file']),
                          {node: secret(path) for node, path in config['node_token_files'].items()},
                          config.get('allow_online_debits', False), config.get('allow_reservations',False),
                          config.get('allow_capacity', False), access,
                          {node: secret(path) for node, path in config.get('identity_token_files', {}).items()})
    except (OSError, ValueError, KeyError, sqlite3.Error):
        raise SystemExit('Cannot initialize operator authority; inspect configuration privately.') from None
    Server((args.bind, args.port), app).serve_forever()


if __name__ == '__main__':
    main()
