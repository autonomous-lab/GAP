"""Dedicated Caddy ingress for managed guests; never configures the shared edge."""
import json
from pathlib import Path
import re
import threading
import http.client
import socket
from urllib.parse import urlsplit

from microvm import VMError


class AdminConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__('localhost', timeout=5)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


class Ingress:
    def __init__(self, config, manager):
        if not manager or config.get('dedicated_caddy') is not True:
            raise ValueError('ingress requires managed VMs and dedicated_caddy=true')
        self.public_url = config.get('public_url', 'https://gap.geta.team').rstrip('/')
        url = urlsplit(self.public_url)
        if (url.scheme not in ('http', 'https') or not url.hostname or url.username
                or url.password or url.path or url.query or url.fragment):
            raise ValueError('public_url must be the existing node origin')
        if any(key in config for key in ('base_domain', 'https_port', 'internal_tls')):
            raise ValueError('use shared-origin public_url; per-project TLS/DNS is unsupported')
        self.admin_socket = config.get('admin_socket', '/run/caddy-admin/admin.sock')
        if ('admin_url' in config or not isinstance(self.admin_socket, str)
                or not re.fullmatch(r'/[A-Za-z0-9_./-]+', self.admin_socket)
                or len(self.admin_socket) > 100 or '..' in self.admin_socket.split('/')):
            raise ValueError('dedicated Caddy requires a safe absolute Unix admin_socket')
        self.http_port = config.get('http_port', 8093)
        if type(self.http_port) is not int or not 1 <= self.http_port <= 65535:
            raise ValueError('invalid ingress http_port')
        token_file=config.get('admission_token_file')
        self.admission_token=Path(token_file).read_text().strip() if token_file else None
        if self.admission_token is not None and (len(self.admission_token)<32 or any(c not in '0123456789abcdef' for c in self.admission_token)):
            raise ValueError('invalid_http_admission_token')
        self.manager = manager
        manager.ingress_origin = self.public_url
        self.lock = threading.Lock()
        self.applied = set()
        self.sync()  # remove stale routes before accepting VM operations

    @staticmethod
    def validate(body):
        if set(body) - {'request_id', 'vm_id', 'enabled', 'guest_port'}:
            raise VMError('invalid_ingress_fields')
        if not re.fullmatch(r'vm_[0-9a-f]{32}', str(body.get('vm_id', ''))):
            raise VMError('vm_id_required')
        if type(body.get('enabled')) is not bool:
            raise VMError('ingress_enabled_required')
        if body['enabled'] and (type(body.get('guest_port')) is not int or not 1 <= body['guest_port'] <= 65535 or body['guest_port'] == 22):
            raise VMError('invalid_ingress_guest_port')
        if not body['enabled'] and 'guest_port' in body:
            raise VMError('disabled_ingress_has_no_port')

    def prefix(self, project):
        self.manager.catalog(project)
        return '/apps/' + project

    def public(self, meta):
        if not meta or meta['state'] == 'destroyed':
            return {'enabled': False, 'routed': False}
        configured = meta.get('ingress', {})
        prefix = self.prefix(meta.get('catalog_key',meta['project_id']))
        return {'enabled': configured.get('enabled', False), 'vm_id': meta['vm_id'],
                'guest_port': configured.get('guest_port'), 'base_path': prefix + '/',
                'url': self.public_url + prefix + '/',
                'routed': meta['vm_id'] in self.applied,
                'note': 'Routing configuration only; not application health'}

    def configuration(self, exclude=None):
        routes, applied = [], set()
        for path in sorted((self.manager.root / 'catalog').glob('*.json')):
            meta = json.loads(path.read_text())
            if exclude in (meta['project_id'],meta['vm_id']) or meta['state'] in ('creating', 'destroyed'):
                continue
            settings = meta.get('ingress', {})
            state=self.manager.public(meta)['state']
            if settings.get('enabled') is not True or (state != 'running' and not (self.manager.runtime and state in ('hibernated','hibernating','resuming'))):
                continue
            port = next((p['worker_port'] for p in meta['ports'] if p['guest_port'] == settings['guest_port']), None)
            if port is None:
                continue
            prefix = self.prefix(meta.get('catalog_key',meta['project_id']))
            routes.append({'match': [{'path': [prefix], 'header': {'X-GAP-VM-Identity': [meta['vm_id']]}}], 'handle': [{
                'handler': 'static_response', 'status_code': 308,
                'headers': {'Location': [prefix + '/{http.request.uri.prefixed_query}']}
            }], 'terminal': True})
            routes.append({'match': [{'path': [prefix + '/*'], 'header': {'X-GAP-VM-Identity': [meta['vm_id']]}}], 'handle': [
                {'handler': 'rewrite', 'strip_path_prefix': prefix},
                {'handler': 'reverse_proxy', 'upstreams': [{'dial': '127.0.0.1:' + str(self.manager.runtime.gateway.port if self.manager.runtime and self.manager.runtime.gateway else port)}],
                 'headers': {'request': {'set': {
                     **({'X-GAP-Project': [meta['project_id']], 'X-GAP-VM': [meta['vm_id']]} if self.manager.runtime else {}),
                     'X-Forwarded-Prefix': [prefix],
                     'X-Forwarded-Proto': [urlsplit(self.public_url).scheme]
                 }, 'delete': ['X-GAP-VM-Admission', 'X-GAP-VM-Identity']}, 'response': {'delete': ['Service-Worker-Allowed']}}}
            ], 'terminal': True})
            applied.add(meta['vm_id'])
        if self.admission_token:
            for route in routes:
                for match in route.get('match',[]):
                    match['header']['X-GAP-VM-Admission']=[self.admission_token]
        else:
            routes=[]; applied=set()
        routes.append({'handle': [{'handler':'static_response','status_code':401,'headers':{'WWW-Authenticate':['Basic realm="GAP microVM"']}}]})
        config = {'admin': {'listen': 'unix/' + self.admin_socket},
                  'apps': {'http': {'servers': {'compose': {
                      'listen': [':' + str(self.http_port)],
                      'automatic_https': {'disable': True}, 'routes': routes}}}}}
        return config, applied

    def sync(self, exclude=None):
        config, applied = self.configuration(exclude)
        connection = AdminConnection(self.admin_socket)
        try:
            connection.request('POST', '/load', body=json.dumps(config).encode(),
                               headers={'Content-Type': 'application/json'})
            response = connection.getresponse()
            if response.status != 200:
                raise ValueError()
            response.read(4096)
        except Exception:
            raise VMError('ingress_apply_failed_retry_after_inspection')
        finally:
            connection.close()
        self.applied = applied

    def perform(self, project, owner, body):
        self.validate(body)
        with self.lock, self.manager.lock(project):
            meta = self.manager.read(project, owner, body['vm_id'])
            if not meta or meta['state'] in ('creating', 'destroyed'):
                raise VMError('vm_not_found')
            if meta['vm_id'] != body['vm_id']:
                raise VMError('vm_generation_mismatch')
            if body['enabled'] and not any(p['guest_port'] == body['guest_port'] for p in meta['ports']):
                raise VMError('ingress_port_not_forwarded_by_vm')
            meta['ingress'] = {'enabled': body['enabled']}
            if body['enabled']:
                meta['ingress']['guest_port'] = body['guest_port']
            self.manager.save(meta)
            self.sync()
            self.manager.sync_environment(meta)
            return {'ok': True, 'ingress': self.public(meta)}

    def vm_operation(self, project, owner, action, body):
        with self.lock:
            # Withdraw routes before any operation can release/reassign a port.
            # A Caddy outage prevents VM mutation rather than leaving stale routes.
            self.sync(exclude=None if self.manager.runtime and action in ('vm/hibernate','vm/resume') else body.get('vm_id'))
            try:
                return self.manager.perform(project, owner, action, body)
            finally:
                self.sync()
