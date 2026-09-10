"""Dedicated Caddy ingress for managed guests; never configures the shared edge."""
import json
import re
import threading
import http.client
import socket

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
        domain = config.get('base_domain', '')
        if (not isinstance(domain, str) or len(domain) > 215 or domain != domain.lower()
                or len(domain.split('.')) < 2 or any(not re.fullmatch(r'[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?', label)
                                                     for label in domain.split('.'))):
            raise ValueError('invalid ingress base_domain')
        self.admin_socket = config.get('admin_socket', '/run/caddy-admin/admin.sock')
        if ('admin_url' in config or not isinstance(self.admin_socket, str)
                or not re.fullmatch(r'/[A-Za-z0-9_./-]+', self.admin_socket)
                or len(self.admin_socket) > 100 or '..' in self.admin_socket.split('/')):
            raise ValueError('dedicated Caddy requires a safe absolute Unix admin_socket')
        self.http_port = config.get('http_port', 80)
        self.https_port = config.get('https_port', 443)
        ports = [self.http_port, self.https_port]
        if any(type(port) is not int or not 1 <= port <= 65535 for port in ports) or len(set(ports)) != 2:
            raise ValueError('ingress ports must be valid and distinct')
        self.internal_tls = config.get('internal_tls', False)
        if type(self.internal_tls) is not bool:
            raise ValueError('internal_tls must be boolean')
        self.domain, self.manager = domain, manager
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

    def host(self, project):
        self.manager.catalog(project)  # validate project before forming hostname
        return project.replace('_', '-', 1) + '.' + self.domain

    def public(self, meta):
        if not meta or meta['state'] == 'destroyed':
            return {'enabled': False, 'routed': False}
        configured = meta.get('ingress', {})
        host = self.host(meta['project_id'])
        suffix = '' if self.https_port == 443 else ':' + str(self.https_port)
        return {'enabled': configured.get('enabled', False), 'vm_id': meta['vm_id'],
                'guest_port': configured.get('guest_port'), 'hostname': host,
                'url': 'https://' + host + suffix,
                'routed': meta['vm_id'] in self.applied,
                'note': 'Routing configuration only; not certificate or application health'}

    def configuration(self, exclude=None):
        routes, names, applied = [], [], set()
        for path in sorted((self.manager.root / 'catalog').glob('prj_*.json')):
            meta = json.loads(path.read_text())
            if meta['project_id'] == exclude or meta['state'] in ('creating', 'destroyed'):
                continue
            settings = meta.get('ingress', {})
            if settings.get('enabled') is not True or self.manager.public(meta)['state'] != 'running':
                continue
            port = next((p['worker_port'] for p in meta['ports'] if p['guest_port'] == settings['guest_port']), None)
            if port is None:
                continue
            host = self.host(meta['project_id'])
            names.append(host)
            routes.append({'match': [{'host': [host]}], 'handle': [{
                'handler': 'reverse_proxy', 'upstreams': [{'dial': '127.0.0.1:' + str(port)}]
            }], 'terminal': True})
            applied.add(meta['vm_id'])
        routes.append({'handle': [{'handler': 'static_response', 'status_code': 404}]})
        config = {'admin': {'listen': 'unix/' + self.admin_socket},
                  'apps': {'http': {'http_port': self.http_port, 'https_port': self.https_port,
                    'servers': {'compose': {'listen': [':' + str(self.https_port)], 'routes': routes}}}}}
        if self.internal_tls and names:
            config['apps']['tls'] = {'automation': {'policies': [{'subjects': names, 'issuers': [{'module': 'internal'}]}]}}
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
            meta = self.manager.read(project, owner)
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
            return {'ok': True, 'ingress': self.public(meta)}

    def vm_operation(self, project, owner, action, body):
        with self.lock:
            # Withdraw routes before any operation can release/reassign a port.
            # A Caddy outage prevents VM mutation rather than leaving stale routes.
            self.sync(exclude=project)
            try:
                return self.manager.perform(project, owner, action, body)
            finally:
                self.sync()
