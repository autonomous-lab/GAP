"""Managed QEMU/KVM microVMs. Host paths and QMP commands are controller-owned.

No agent-supplied command, host mount, disk path, kernel or SSH target is used.
The runner requires KVM access but not host root, Docker or network-admin rights.
"""
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import time
import uuid


class VMError(Exception):
    pass


def atomic_json(path, value):
    temporary = path.with_suffix('.tmp')
    with temporary.open('w') as out:
        json.dump(value, out)
        out.flush()
        os.fsync(out.fileno())
    temporary.replace(path)
    directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def run(command):
    # Never execute a shell, inherit secrets or return host command output.
    result = subprocess.run(command, env={'PATH': '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin'},
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                            stderr=subprocess.PIPE, timeout=120)
    if result.returncode:
        error = VMError('hypervisor_command_failed:' + Path(command[0]).name)
        error.diagnostic = result.stderr[-4096:].decode(errors='replace')
        raise error


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def validate(action, body):
    fields = {
        'vm/create': {'request_id', 'vcpus', 'memory_mib', 'disk_gib', 'ports', 'start', 'ssh_keys'},
        'vm/update': {'request_id', 'vm_id', 'vcpus', 'memory_mib', 'disk_gib', 'ports'},
        'vm/start': {'request_id', 'vm_id'},
        'vm/stop': {'request_id', 'vm_id', 'force'},
        'vm/destroy': {'request_id', 'vm_id', 'delete_data', 'confirm_data_loss'},
    }
    if action not in fields or set(body) - fields[action]:
        raise VMError('invalid_vm_operation_fields')
    if action != 'vm/create' and not re.fullmatch(r'vm_[0-9a-f]{32}', str(body.get('vm_id', ''))):
        raise VMError('vm_id_required')
    for key in ('vcpus', 'memory_mib', 'disk_gib'):
        if key in body and (type(body[key]) is not int or not 0 < body[key] < 2**31):
            raise VMError('invalid_' + key)
    for key in ('start', 'force', 'delete_data', 'confirm_data_loss'):
        if key in body and type(body[key]) is not bool:
            raise VMError('invalid_' + key)
    if 'ssh_keys' in body:
        from network import keys
        keys(body['ssh_keys'])
    ports = body.get('ports', [])
    if (not isinstance(ports, list) or any(type(p) is not int or not 1 <= p <= 65535 or p == 22 for p in ports)
            or len(set(ports)) != len(ports)):
        raise VMError('invalid_guest_ports')
    if body.get('delete_data') and body.get('confirm_data_loss') is not True:
        raise VMError('confirm_data_loss_required')


class MicroVMs:
    def __init__(self, config, execute_guest):
        self.root = Path(config['state_dir']).resolve()
        self.images = Path(config['image_dir']).resolve()
        # QEMU option strings and UNIX socket paths must stay unambiguous.
        if not re.fullmatch(r'/[A-Za-z0-9_./-]+', str(self.root)) or len(str(self.root)) > 45:
            raise VMError('hypervisor_state_dir_must_be_short_absolute_safe_path')
        if not re.fullmatch(r'/[A-Za-z0-9_./-]+', str(self.images)):
            raise VMError('invalid_image_dir')
        self.execute_guest = execute_guest
        self.diagnostic_serial = config.get('diagnostic_serial', False)
        self.children = {}
        self.ingress_origin = ''
        self.network = None
        if config.get('public_network'):
            from network import Network
            self.network = Network(self, config['public_network'])
        for folder in ('catalog', 'vms', 'retained', 'locks'):
            (self.root / folder).mkdir(parents=True, exist_ok=True, mode=0o700)

    @contextmanager
    def lock(self, project):
        if not re.fullmatch(r'prj_[0-9a-f]{24}', project):
            raise VMError('invalid_project')
        with (self.root / 'locks' / project).open('a') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise VMError('vm_operation_in_progress')
            yield

    @contextmanager
    def allocation_lock(self):
        with (self.root / 'locks' / 'port-allocation').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            yield

    def catalog(self, project):
        if not re.fullmatch(r'prj_[0-9a-f]{24}', project):
            raise VMError('invalid_project')
        return self.root / 'catalog' / (project + '.json')

    def read(self, project, owner):
        path = self.catalog(project)
        if not path.exists():
            return None
        meta = json.loads(path.read_text())
        if meta['owner_did'] != owner:
            raise VMError('vm_owner_mismatch')
        return meta

    def folder(self, meta):
        vm_id = meta['vm_id']
        if not re.fullmatch(r'vm_[0-9a-f]{32}', vm_id):
            raise VMError('invalid_vm_identity')
        path = self.root / 'vms' / vm_id
        if path.is_symlink() or path.resolve().parent != self.root / 'vms':
            raise VMError('unsafe_vm_path')
        return path

    def save(self, meta):
        atomic_json(self.catalog(meta['project_id']), meta)

    def qmp(self, meta, command, arguments=None):
        if command not in ('query-status', 'quit', 'human-monitor-command'):
            raise VMError('invalid_qmp_command')
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(3)
            sock.connect(str(self.folder(meta) / 'qmp.sock'))
            with sock.makefile('rwb') as io:
                def read():
                    raw = io.readline(65537)
                    if not raw or len(raw) > 65536:
                        raise VMError('invalid_qmp_response')
                    return json.loads(raw)
                if 'QMP' not in read():
                    raise VMError('invalid_qmp_greeting')
                def call(name, identity, args=None):
                    message = {'execute': name, 'id': identity}
                    if args is not None:
                        message['arguments'] = args
                    io.write((json.dumps(message) + '\n').encode())
                    io.flush()
                    for _ in range(64):
                        response = read()
                        if response.get('id') == identity:
                            if 'error' in response:
                                raise VMError('qmp_command_failed')
                            return response['return']
                    raise VMError('qmp_event_overflow')
                call('qmp_capabilities', 1)
                if call('query-name', 2).get('name') != meta['vm_id']:
                    raise VMError('qmp_vm_identity_mismatch')
                return call(command, 3, arguments)

    def alive(self, meta):
        # This probe is only for deciding whether deletion/start is safe.
        # Never signal a PID: PIDs can be reused. Mutation uses verified QMP.
        pidfile = self.folder(meta) / 'qemu.pid'
        if not pidfile.exists():
            return False
        try:
            pid = int(pidfile.read_text())
            raw = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0')
            return meta['vm_id'].encode() in raw and str(self.folder(meta) / 'disk.qcow2').encode() in b'\0'.join(raw)
        except (FileNotFoundError, ProcessLookupError):
            return False
        except (ValueError, PermissionError):
            raise VMError('cannot_establish_vm_process_identity')

    def public(self, meta):
        if not meta:
            return {'state': 'absent'}
        result = {key: meta[key] for key in ('vm_id', 'project_id', 'state', 'vcpus', 'memory_mib', 'disk_gib', 'ports')}
        if meta['state'] not in ('destroyed', 'creating'):
            try:
                result['state'] = self.qmp(meta, 'query-status')['status']
            except (OSError, VMError):
                result['state'] = 'unknown' if self.alive(meta) else 'stopped'
        result['public_ports'] = meta.get('public_ports', [])
        if self.network:
            result['public_hostname'] = self.network.host
        result['environment_sync_pending'] = meta.get('environment_sync_pending', False)
        if meta.get('retained'):
            result['retained_volume_id'] = meta['vm_id']
        return result

    def guest(self, project, owner):
        meta = self.read(project, owner)
        if not meta or self.public(meta)['state'] != 'running':
            raise VMError('vm_not_running')
        folder = self.folder(meta)
        return {'vm_id': meta['vm_id'], 'address': '127.0.0.1', 'port': meta['ssh_port'],
                'ssh_key': str(folder / 'client_key'), 'known_hosts': str(folder / 'known_hosts')}

    def image_version(self):
        manifest = (self.images / 'SHA256SUMS').read_text()
        seen = set()
        for line in manifest.splitlines():
            digest, name = line.split()
            if name not in ('vmlinuz', 'initramfs', 'rootfs.ext4') or name in seen:
                raise VMError('invalid_guest_image_manifest')
            seen.add(name)
            hasher = hashlib.sha256()
            with (self.images / name).open('rb') as source:
                for chunk in iter(lambda: source.read(1024 * 1024), b''):
                    hasher.update(chunk)
            if hasher.hexdigest() != digest:
                raise VMError('guest_image_checksum_mismatch')
        if len(manifest.splitlines()) != 3:
            raise VMError('invalid_guest_image_manifest')
        return hashlib.sha256(manifest.encode()).hexdigest()

    def prepare(self, meta):
        folder = self.folder(meta)
        folder.mkdir(mode=0o700)
        meta['image_version'] = self.image_version()
        run(['qemu-img', 'create', '-f', 'qcow2', '-F', 'raw', '-b', str(self.images / 'rootfs.ext4'),
             str(folder / 'disk.qcow2'), str(meta['disk_gib']) + 'G'])
        seed = folder / 'seed'
        seed.mkdir(mode=0o700)
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(folder / 'client_key')])
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(seed / 'ssh_host_ed25519_key')])
        (seed / 'authorized_keys').write_text(self.authorized_keys(meta, meta.get('ssh_keys', [])))
        (folder / 'known_hosts').write_text(meta['vm_id'] + ' ' + (seed / 'ssh_host_ed25519_key.pub').read_text())
        (seed / 'runtime.json').write_text(json.dumps(self.environment(meta)))
        with (folder / 'seed.ext4').open('wb') as image:
            image.truncate(4 * 1024 * 1024)
        run(['mkfs.ext4', '-q', '-F', '-d', str(seed), str(folder / 'seed.ext4')])
        # Seed contains this guest's identity, never the client's private key.

    def environment(self, meta):
        from environment import variables
        return variables(meta, self.network.host if self.network else '', self.ingress_origin)

    def sync_environment(self, meta):
        values = self.environment(meta)
        meta['environment_sync_pending'] = True
        self.save(meta)
        (self.folder(meta) / 'seed' / 'runtime.json').write_text(json.dumps(values))
        if self.alive(meta):
            result = self.execute_guest(self.guest(meta['project_id'], meta['owner_did']),
                {'action': 'runtime_environment', 'body': {'variables': values}}, timeout=15)
            if result.get('ok') is not True:
                raise VMError('guest_environment_update_failed')
        meta['environment_sync_pending'] = False
        self.save(meta)

    def refresh_seed(self, meta):
        folder = self.folder(meta)
        (folder / 'seed' / 'authorized_keys').write_text(self.authorized_keys(meta, meta.get('ssh_keys', [])))
        (folder / 'seed' / 'runtime.json').write_text(json.dumps(self.environment(meta)))
        temporary = folder / 'seed-next.ext4'
        with temporary.open('wb') as image:
            image.truncate(4 * 1024 * 1024)
        run(['mkfs.ext4', '-q', '-F', '-d', str(folder / 'seed'), str(temporary)])
        temporary.replace(folder / 'seed.ext4')

    def authorized_keys(self, meta, keys):
        return ('restrict,command="python3 /usr/local/lib/gap-compose-guest.py" ' +
                (self.folder(meta) / 'client_key.pub').read_text().strip() + '\n' +
                ''.join('no-agent-forwarding,no-X11-forwarding ' + key + '\n' for key in keys))

    def write_keys(self, meta, keys):
        folder = self.folder(meta)
        content = self.authorized_keys(meta, keys)
        # Prepare a replacement seed; never modify a mounted seed image in place.
        (folder / 'seed' / 'authorized_keys').write_text(content)
        temporary = folder / 'seed-next.ext4'
        with temporary.open('wb') as image:
            image.truncate(4 * 1024 * 1024)
        run(['mkfs.ext4', '-q', '-F', '-d', str(folder / 'seed'), str(temporary)])
        if self.alive(meta):
            result = self.execute_guest(self.guest(meta['project_id'], meta['owner_did']),
                                        {'action': 'ssh_keys', 'body': {'content': content}}, timeout=15)
            if result.get('ok') is not True:
                raise VMError('ssh_keys_update_failed')
        temporary.replace(folder / 'seed.ext4')

    def command(self, meta):
        folder = self.folder(meta)
        forwards = [f'hostfwd=tcp:127.0.0.1:{meta["ssh_port"]}-:22']
        forwards += [f'hostfwd=tcp:127.0.0.1:{port["worker_port"]}-:{port["guest_port"]}' for port in meta['ports']]
        if self.network:
            forwards += ['hostfwd=' + rule for rule in self.network.rules(meta)]
        return ['qemu-system-x86_64', '-machine', 'microvm,accel=kvm', '-cpu', 'host',
                '-name', meta['vm_id'], '-m', str(meta['memory_mib']), '-smp', str(meta['vcpus']),
                '-kernel', str(self.images / 'vmlinuz'), '-initrd', str(self.images / 'initramfs'),
                '-append', 'console=ttyS0 root=/dev/vda rootfstype=ext4 modules=virtio_mmio,virtio_blk,ext4 rootwait rw reboot=t net.ifnames=0',
                '-nodefaults', '-no-user-config', '-display', 'none',
                '-serial', f'file:{folder}/serial.log' if self.diagnostic_serial else 'null', '-no-reboot',
                '-sandbox', 'on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny',
                '-drive', f'id=root,file={folder}/disk.qcow2,format=qcow2,if=none',
                '-device', 'virtio-blk-device,drive=root',
                '-drive', f'id=seed,file={folder}/seed.ext4,format=raw,if=none,readonly=on',
                '-device', 'virtio-blk-device,drive=seed',
                '-device', 'virtio-rng-device',
                '-netdev', 'user,id=net0,' + ','.join(forwards), '-device', 'virtio-net-device,netdev=net0',
                '-qmp', f'unix:{folder}/qmp.sock,server=on,wait=off',
                '-pidfile', str(folder / 'qemu.pid')]

    def start(self, meta):
        if meta['state'] == 'creating':
            raise VMError('vm_creation_incomplete_destroy_and_retry')
        if self.public(meta)['state'] == 'running':
            return
        if self.alive(meta):
            raise VMError('vm_process_alive_but_unresponsive')
        if self.image_version() != meta['image_version']:
            raise VMError('guest_base_image_changed')
        folder = self.folder(meta)
        self.refresh_seed(meta)
        error_log = folder / 'hypervisor.log'
        with (error_log.open('wb') if self.diagnostic_serial else open(os.devnull, 'wb')) as log:
            process = subprocess.Popen(self.command(meta), stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, stderr=log, start_new_session=True,
                env={'PATH': '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin'})
        self.children[meta['vm_id']] = process
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if process.poll() is not None:
                error = VMError('hypervisor_start_failed')
                error.diagnostic = error_log.read_text(errors='replace')[-4096:] if error_log.exists() else ''
                raise error
            try:
                if self.qmp(meta, 'query-status')['status'] == 'running':
                    break
            except OSError:
                pass
            time.sleep(.1)
        else:
            process.terminate()
            process.wait(timeout=10)
            raise VMError('hypervisor_start_timeout')
        meta['state'] = 'running'
        meta['network_pending'] = False
        meta['environment_sync_pending'] = False
        self.save(meta)

    def stop(self, meta, force):
        if not self.alive(meta):
            meta['state'] = 'stopped'
            self.save(meta)
            return
        if force:
            self.qmp(meta, 'quit')
        else:
            self.execute_guest(self.guest(meta['project_id'], meta['owner_did']),
                               {'action': 'vm_shutdown', 'body': {}}, timeout=15)
        deadline = time.monotonic() + 30
        while self.alive(meta) and time.monotonic() < deadline:
            time.sleep(.1)
        if self.alive(meta):
            raise VMError('vm_still_running_use_explicit_force_stop')
        meta['state'] = 'stopped'
        self.save(meta)
        child = self.children.pop(meta['vm_id'], None)
        if child:
            child.wait(timeout=5)

    def perform(self, project, owner, action, body):
        validate(action, body)
        with self.lock(project):
            meta = self.read(project, owner)
            if action == 'vm/create':
                if meta and meta['state'] != 'destroyed':
                    raise VMError('vm_already_exists')
                meta = {'vm_id': 'vm_' + uuid.uuid4().hex, 'project_id': project, 'owner_did': owner,
                        'state': 'creating', 'vcpus': body.get('vcpus', 1),
                        'memory_mib': body.get('memory_mib', 1024), 'disk_gib': body.get('disk_gib', 8),
                        'ssh_port': free_port(), 'ports': [], 'retained': False,
                        'ssh_keys': __import__('network').keys(body.get('ssh_keys', []))}
                used = {meta['ssh_port']}
                for port in body.get('ports', []):
                    candidate = free_port()
                    while candidate in used:
                        candidate = free_port()
                    used.add(candidate)
                    meta['ports'].append({'guest_port': port, 'worker_port': candidate})
                with self.allocation_lock():
                    if self.network:
                        self.network.allocate(meta)
                    self.save(meta)  # durable reservation before creating disks/processes
                self.prepare(meta)
                meta['state'] = 'stopped'
                self.save(meta)
                if body.get('start', True):
                    self.start(meta)
            else:
                if not meta or meta['state'] == 'destroyed':
                    raise VMError('vm_not_found')
                if body['vm_id'] != meta['vm_id']:
                    raise VMError('vm_generation_mismatch')
                if action == 'vm/start':
                    self.start(meta)
                elif action == 'vm/stop':
                    self.stop(meta, body.get('force', False))
                elif action == 'vm/update':
                    if self.alive(meta):
                        raise VMError('stop_vm_before_reconfiguration')
                    if body.get('disk_gib', meta['disk_gib']) < meta['disk_gib']:
                        raise VMError('disk_shrink_not_supported')
                    if body.get('disk_gib', meta['disk_gib']) > meta['disk_gib']:
                        run(['qemu-img', 'resize', str(self.folder(meta) / 'disk.qcow2'), str(body['disk_gib']) + 'G'])
                    for key in ('vcpus', 'memory_mib', 'disk_gib'):
                        meta[key] = body.get(key, meta[key])
                    if 'ports' in body:
                        used = {meta['ssh_port']}
                        meta['ports'] = []
                        for port in body['ports']:
                            candidate = free_port()
                            while candidate in used:
                                candidate = free_port()
                            used.add(candidate)
                            meta['ports'].append({'guest_port': port, 'worker_port': candidate})
                    self.save(meta)
                elif action == 'vm/destroy':
                    if self.alive(meta):
                        raise VMError('stop_vm_before_destruction')
                    folder = self.folder(meta)
                    if folder.exists():
                        if body.get('delete_data', False):
                            # Exact generated ID under a validated controller root.
                            shutil.rmtree(folder)
                        else:
                            folder.rename(self.root / 'retained' / meta['vm_id'])
                    meta['state'] = 'destroyed'
                    meta['retained'] = not body.get('delete_data', False)
                    self.save(meta)
            return {'ok': True, 'vm': self.public(meta)}
