"""Five durable public port numbers per VM; only typed QEMU forwarding rules."""
import base64
import hashlib
import re
import socket

from microvm import VMError


def keys(value):
    # A single unadorned Ed25519 key per line; never accept authorized_keys options.
    if not isinstance(value, list) or len(value) > 20:
        raise VMError('invalid_ssh_keys')
    result = []
    for key in value:
        if not isinstance(key, str) or len(key) > 1024 or '\n' in key or '\r' in key:
            raise VMError('invalid_ssh_key')
        parts = key.split()
        try:
            raw = base64.b64decode(parts[1], validate=True)
            if parts[0] != 'ssh-ed25519' or raw[:19] != b'\x00\x00\x00\x0bssh-ed25519\x00\x00\x00\x20' or len(raw) != 51:
                raise ValueError()
        except (ValueError, IndexError):
            raise VMError('expected_ed25519_public_key')
        normalized = 'ssh-ed25519 ' + parts[1]
        if normalized not in result:
            result.append(normalized)
    return result


def validate(action, body):
    field = 'mappings' if action == 'ports' else 'authorized_keys'
    if set(body) != {'request_id', 'vm_id', field} or not re.fullmatch(r'vm_[0-9a-f]{32}', str(body.get('vm_id', ''))):
        raise VMError('invalid_' + action + '_fields')
    if action == 'ssh':
        keys(body[field])
        return
    if not isinstance(body[field], list) or len(body[field]) > 5:
        raise VMError('maximum_five_public_ports')
    seen, udp_targets = set(), set()
    for item in body[field]:
        if (not isinstance(item, dict) or set(item) != {'slot', 'guest_port', 'protocol'}
                or type(item['slot']) is not int or not 1 <= item['slot'] <= 5
                or type(item['guest_port']) is not int or not 1 <= item['guest_port'] <= 65535
                or item['protocol'] not in ('tcp', 'udp', 'both') or item['slot'] in seen):
            raise VMError('invalid_public_port_mapping')
        seen.add(item['slot'])
        if item['protocol'] in ('udp', 'both'):
            if item['guest_port'] in udp_targets:
                raise VMError('duplicate_udp_guest_port')
            udp_targets.add(item['guest_port'])


class Network:
    def __init__(self, manager, config):
        self.manager = manager
        self.host = config['hostname']
        self.first, self.last = config['first_port'], config['last_port']
        if (not isinstance(self.host, str) or not re.fullmatch(r'[a-z0-9]+(?:[.-][a-z0-9]+)*', self.host)
                or type(self.first) is not int or type(self.last) is not int
                or not 1024 <= self.first <= self.last <= 65535 or self.last - self.first < 4):
            raise VMError('invalid_public_network_config')

    def allocate(self, meta):
        # Caller holds the cross-project catalog allocation lock until save.
        import json
        used = set()
        for path in (self.manager.root / 'catalog').glob('prj_*.json'):
            other = json.loads(path.read_text())
            if other['state'] != 'destroyed':
                used.update(other.get('public_ports', []))
        available = []
        for port in range(self.first, self.last + 1):
            if port in used:
                continue
            try:
                with socket.socket() as tcp, socket.socket(type=socket.SOCK_DGRAM) as udp:
                    tcp.bind(('0.0.0.0', port))
                    udp.bind(('0.0.0.0', port))
            except OSError:
                continue
            available.append(port)
            if len(available) == 5:
                meta['public_ports'] = available
                meta['public_mappings'] = []
                return
        raise VMError('public_port_pool_exhausted')

    def public(self, meta):
        if not meta or meta['state'] == 'destroyed':
            return {'hostname': self.host, 'ports': [], 'state': 'absent'}
        configured = {p['slot']: p for p in meta.get('public_mappings', [])}
        running = self.manager.public(meta)['state'] == 'running'
        return {'vm_id': meta['vm_id'], 'hostname': self.host,
                'state': 'pending' if meta.get('network_pending') else ('running' if running else 'stopped'),
                'ports': [dict(slot=i, public_port=p, guest_port=configured.get(i, {}).get('guest_port'),
                               protocol=configured.get(i, {}).get('protocol'),
                               routed=running and not meta.get('network_pending', False) and i in configured)
                          for i, p in enumerate(meta.get('public_ports', []), 1)]}

    def ssh_public(self, meta):
        if not meta or meta['state'] == 'destroyed':
            return {'authorized_keys': [], 'connections': []}
        host_key = self.manager.folder(meta) / 'seed' / 'ssh_host_ed25519_key.pub'
        public = host_key.read_text().strip() if host_key.exists() else None
        fingerprint = ('SHA256:' + base64.b64encode(hashlib.sha256(base64.b64decode(public.split()[1])).digest()).decode().rstrip('=')) if public else None
        return {'vm_id': meta['vm_id'], 'username': 'root', 'authorized_keys': meta.get('ssh_keys', []),
                'host_public_key': public, 'host_key_fingerprint': fingerprint,
                'connections': [{'hostname': self.host, 'port': p['public_port'],
                                 'command': f'ssh -p {p["public_port"]} root@{self.host}'}
                                for p in self.public(meta)['ports'] if p['guest_port'] == 22 and p['protocol'] in ('tcp', 'both')]}

    def rules(self, meta):
        for mapping in meta.get('public_mappings', []):
            port = meta['public_ports'][mapping['slot'] - 1]
            for protocol in (('tcp', 'udp') if mapping['protocol'] == 'both' else (mapping['protocol'],)):
                yield f'{protocol}:0.0.0.0:{port}-:{mapping["guest_port"]}'

    def apply(self, meta):
        # Remove every reserved listener first, including a previous partial apply.
        # Existing sessions may survive removal; stopping the VM terminates them.
        for port in meta['public_ports']:
            for protocol in ('tcp', 'udp'):
                response = self.manager.qmp(meta, 'human-monitor-command',
                    {'command-line': f'hostfwd_remove net0 {protocol}:0.0.0.0:{port}'})
                expected = f'host forwarding rule for {protocol}:0.0.0.0:{port} '
                if response.strip() not in (expected + 'removed', expected + 'not found'):
                    raise VMError('public_port_remove_failed')
        for rule in self.rules(meta):
            response = self.manager.qmp(meta, 'human-monitor-command', {'command-line': 'hostfwd_add net0 ' + rule})
            if response.strip():
                raise VMError('public_port_apply_failed')

    def perform(self, project, owner, action, body):
        validate(action, body)
        with self.manager.lock(project):
            meta = self.manager.read(project, owner)
            if not meta or meta['state'] in ('creating', 'destroyed'):
                raise VMError('vm_not_found')
            if meta['vm_id'] != body['vm_id']:
                raise VMError('vm_generation_mismatch')
            if action == 'ssh':
                new_keys = keys(body['authorized_keys'])
                self.manager.write_keys(meta, new_keys)
                meta['ssh_keys'] = new_keys
                self.manager.save(meta)
                return {'ok': True, 'ssh': self.ssh_public(meta)}
            if not meta.get('public_ports'):
                with self.manager.allocation_lock():
                    self.allocate(meta)
                    self.manager.save(meta)
            meta['public_mappings'] = body['mappings']
            meta['network_pending'] = True
            self.manager.save(meta)
            if self.manager.alive(meta):
                self.apply(meta)
            meta['network_pending'] = False
            self.manager.save(meta)
            return {'ok': True, 'ports': self.public(meta)}
