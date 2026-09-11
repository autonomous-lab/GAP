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
        'vm/hibernate': {'request_id', 'vm_id'},
        'vm/resume': {'request_id', 'vm_id'},
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
        self.quota_provider = lambda project, owner: {"vcpus": 2, "memory_mib": 4096}
        self.runtime = None
        self.meters = {}
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

    def reserved_port(self, used=()):
        # Called under allocation_lock before publishing metadata. Include
        # hibernated guests: their unbound endpoints must never be reassigned.
        occupied=set(used)
        for path in (self.root/'catalog').glob('prj_*.json'):
            other=json.loads(path.read_text())
            if other['state']=='destroyed': continue
            occupied.add(other['ssh_port'])
            occupied.update(p['worker_port'] for p in other.get('ports',[]))
            occupied.update(other.get('public_targets',{}).values())
            occupied.update(other.get('public_ports',[]))
        for _ in range(1000):
            port=free_port()
            if port not in occupied: return port
        raise VMError('internal_port_pool_exhausted')

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
        if command not in ('query-status', 'quit', 'human-monitor-command', 'stop', 'cont'):
            raise VMError('invalid_qmp_command')
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(120 if command == 'human-monitor-command' else 3)
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
        if meta['state'] not in ('destroyed', 'creating', 'hibernated', 'hibernating', 'resuming'):
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
        command = ['qemu-system-x86_64', '-machine', 'microvm,accel=kvm', '-cpu', 'host',
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
                '-netdev', 'user,id=net0,' + ','.join(forwards), '-device', 'virtio-net-device,netdev=net0,mac=' + self.guest_mac(meta),
                '-qmp', f'unix:{folder}/qmp.sock,server=on,wait=off',
                '-pidfile', str(folder / 'qemu.pid')]
        if self.runtime:
            command += ['-object', f'filter-dump,id=meterin,netdev=net0,queue=tx,file={folder}/meter-in.fifo,maxlen=96',
                        '-object', f'filter-dump,id=meterout,netdev=net0,queue=rx,file={folder}/meter-out.fifo,maxlen=96']
        return command

    @staticmethod
    def guest_mac(meta):
        return '52:54:' + ':'.join(meta['vm_id'][3+i:5+i] for i in (0,2,4,6))

    def capture_start(self, meta):
        if self.runtime:
            from meter import PacketMeter
            self.capture_stop(meta)
            self.meters[meta['vm_id']] = PacketMeter(self.folder(meta), self.guest_mac(meta))

    def capture_stop(self, meta):
        meter = self.meters.pop(meta['vm_id'], None)
        if meter: meter.close()

    def hibernate(self, meta):
        if meta['state'] == 'hibernated': return
        if self.public(meta)['state'] != 'running':
            raise VMError('hibernate_requires_running_vm')
        if shutil.disk_usage(self.folder(meta)).free < meta['memory_mib']*1024**2+512*1024**2:
            raise VMError('insufficient_disk_for_hibernation')
        meta['snapshot_qemu_version']=subprocess.check_output(['qemu-system-x86_64','--version'],text=True).splitlines()[0]
        tag='idle_' + uuid.uuid4().hex
        meta.update(state='hibernating', snapshot_tag=tag)
        self.save(meta)
        try:
            self.qmp(meta,'stop')
            response=self.qmp(meta,'human-monitor-command',{'command-line':'savevm '+tag})
            if response.strip(): raise VMError('snapshot_save_failed')
            self.qmp(meta,'quit')
            child=self.children.pop(meta['vm_id'],None)
            if child: child.wait(timeout=15)
            deadline=time.monotonic()+15
            while self.alive(meta) and time.monotonic()<deadline: time.sleep(.05)
            if self.alive(meta): raise VMError('hibernate_process_still_alive')
            self.capture_stop(meta)
            meta.update(state='hibernated',hibernated_at=int(time.time()))
            self.save(meta)
        except Exception:
            if self.alive(meta):
                self.qmp(meta,'cont')
                meta['state']='running'; self.save(meta)
            raise

    def resume(self, meta):
        if meta['state'] != 'hibernated':
            return self.start(meta)
        tag=meta.get('snapshot_tag','')
        if not re.fullmatch(r'idle_[0-9a-f]{32}',tag): raise VMError('invalid_snapshot_identity')
        if self.image_version()!=meta['image_version']: raise VMError('guest_base_image_changed')
        if meta.get('snapshot_qemu_version')!=subprocess.check_output(['qemu-system-x86_64','--version'],text=True).splitlines()[0]:
            raise VMError('snapshot_requires_original_qemu_version')
        meta['state']='resuming'; self.save(meta)
        self.capture_start(meta)
        log=self.folder(meta)/'restore.log'
        with log.open('wb') as error:
            process=subprocess.Popen(self.command(meta)+['-loadvm',tag,'-S'],stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,stderr=error,env={'PATH':'/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin'})
        self.children[meta['vm_id']]=process
        deadline=time.monotonic()+90
        try:
            while time.monotonic()<deadline:
                if process.poll() is not None: raise VMError('snapshot_restore_failed')
                try:
                    if self.qmp(meta,'query-status')['status'] in ('paused','prelaunch'): break
                except OSError: pass
                time.sleep(.01)
            else: raise VMError('snapshot_restore_timeout')
            # Mark resumed before executing guest instructions: never replay an old
            # memory snapshot after a crash following an externally visible write.
            meta['state']='running'; self.save(meta)
            self.qmp(meta,'cont')
            response=self.qmp(meta,'human-monitor-command',{'command-line':'delvm '+tag})
            if response.strip(): meta['snapshot_cleanup_pending']=True
            else: meta.pop('snapshot_tag',None)
            self.save(meta)
        except Exception:
            if process.poll() is None:
                process.terminate(); process.wait(timeout=10)
            self.capture_stop(meta)
            if meta['state']=='resuming': meta['state']='hibernated'
            else: meta['state']='stopped'
            self.save(meta)
            raise

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
        self.capture_start(meta)
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
            try: self.qmp(meta, 'quit')
            except (OSError,VMError):
                # Only signal an unreaped child owned by this worker. Never
                # trust an arbitrary/reused PID read from disk.
                child=self.children.get(meta['vm_id'])
                if child is None or child.poll() is not None: raise
                child.kill(); child.wait(timeout=10)
        else:
            self.execute_guest(self.guest(meta['project_id'], meta['owner_did']),
                               {'action': 'vm_shutdown', 'body': {}}, timeout=15)
        deadline = time.monotonic() + 30
        while self.alive(meta) and time.monotonic() < deadline:
            time.sleep(.1)
        if self.alive(meta):
            raise VMError('vm_still_running_use_explicit_force_stop')
        self.capture_stop(meta)
        meta['state'] = 'stopped'
        self.save(meta)
        child = self.children.pop(meta['vm_id'], None)
        if child:
            child.wait(timeout=5)

    @contextmanager
    def owner_lock(self, owner):
        if not re.fullmatch(r'did:gap:[0-9a-f]{64}', owner):
            raise VMError('invalid_owner_identity')
        with (self.root / 'locks' / ('owner-' + owner[8:])).open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            yield

    def quota_usage(self, owner):
        usage = {'vcpus': 0, 'memory_mib': 0}
        for path in (self.root / 'catalog').glob('*.json'):
            meta = json.loads(path.read_text())
            if meta['owner_did'] == owner and meta['state'] != 'destroyed':
                for key in usage:
                    value = meta[key]
                    if type(value) is not int or value <= 0:
                        raise VMError('invalid_resource_catalog')
                    usage[key] += value
        return usage

    def quota_view(self, project, owner):
        with self.owner_lock(owner):
            return {'limits': self.quota_provider(project, owner), 'allocated': self.quota_usage(owner)}

    def perform(self, project, owner, action, body):
        validate(action, body)
        # All lifecycle changes share an owner lock across projects and processes.
        # Read live approval/quota after acquiring it, before reserving resources.
        with self.owner_lock(owner):
            limits = self.quota_provider(project, owner)
            if (not isinstance(limits, dict) or set(limits) != {'vcpus', 'memory_mib'}
                    or any(type(v) is not int or not 0 < v < 2**31 for v in limits.values())):
                raise VMError('invalid_agent_quota')
            meta = self.read(project, owner)
            if action in ('vm/create', 'vm/update', 'vm/start', 'vm/resume'):
                usage = self.quota_usage(owner)
                for key, default in (('vcpus', 1), ('memory_mib', 1024)):
                    if action == 'vm/create':
                        requested, previous = body.get(key, default), 0
                    elif meta and meta['state'] != 'destroyed':
                        previous = meta[key]
                        requested = body.get(key, previous) if action == 'vm/update' else previous
                    else:
                        continue
                    total = usage[key] - previous + requested
                    # After a quota reduction, allow releases and reductions, never growth.
                    if total > limits[key] and (action != 'vm/update' or requested > previous):
                        raise VMError('agent_quota_exceeded_' + key)
            return self._perform(project, owner, action, body)

    def _perform(self, project, owner, action, body):
        validate(action, body)
        with self.lock(project):
            meta = self.read(project, owner)
            if action == 'vm/create':
                if meta and meta['state'] != 'destroyed':
                    raise VMError('vm_already_exists')
                with self.allocation_lock():
                    meta = {'vm_id': 'vm_' + uuid.uuid4().hex, 'project_id': project, 'owner_did': owner,
                            'state': 'creating', 'vcpus': body.get('vcpus', 1),
                            'memory_mib': body.get('memory_mib', 1024), 'disk_gib': body.get('disk_gib', 8),
                            'ssh_port': self.reserved_port(), 'ports': [], 'retained': False,
                            'ssh_keys': __import__('network').keys(body.get('ssh_keys', []))}
                    used = {meta['ssh_port']}
                    for port in body.get('ports', []):
                        candidate = self.reserved_port(used)
                        while candidate in used:
                            candidate = self.reserved_port(used)
                        used.add(candidate)
                        meta['ports'].append({'guest_port': port, 'worker_port': candidate})
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
                if action in ('vm/start','vm/resume'):
                    self.resume(meta) if meta['state']=='hibernated' else self.start(meta)
                elif action == 'vm/hibernate':
                    self.hibernate(meta)
                elif action == 'vm/stop':
                    self.stop(meta, body.get('force', False))
                elif action == 'vm/update':
                    if meta['state']=='hibernated': raise VMError('resume_then_stop_before_resize')
                    if self.alive(meta):
                        raise VMError('stop_vm_before_reconfiguration')
                    if body.get('disk_gib', meta['disk_gib']) < meta['disk_gib']:
                        raise VMError('disk_shrink_not_supported')
                    if body.get('disk_gib', meta['disk_gib']) > meta['disk_gib']:
                        run(['qemu-img', 'resize', str(self.folder(meta) / 'disk.qcow2'), str(body['disk_gib']) + 'G'])
                    for key in ('vcpus', 'memory_mib', 'disk_gib'):
                        meta[key] = body.get(key, meta[key])
                    with self.allocation_lock():
                        if 'ports' in body:
                            used = {meta['ssh_port']}
                            meta['ports'] = []
                            for port in body['ports']:
                                candidate = self.reserved_port(used)
                                while candidate in used:
                                    candidate = self.reserved_port(used)
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
                            atomic_json(folder/'retention.json',{'project_id':project,'owner_did':owner,'vm_id':meta['vm_id']})
                            folder.rename(self.root / 'retained' / meta['vm_id'])
                    meta['state'] = 'destroyed'
                    meta['retained'] = not body.get('delete_data', False)
                    self.save(meta)
            return {'ok': True, 'vm': self.public(meta)}
