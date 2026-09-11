"""Non-secret guest runtime environment, shared by controller and guest installer."""
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import sys
import tempfile


def variables(meta, hostname='', origin=''):
    settings = meta.get('ingress', {})
    enabled = bool(origin and settings.get('enabled') and any(
        p['guest_port'] == settings.get('guest_port') for p in meta['ports']))
    base_path = '/apps/' + meta['project_id'] + '/' if origin else ''
    url = origin + base_path if enabled else ''
    mappings = {p['slot']: p for p in meta.get('public_mappings', [])}
    ports = [dict(slot=i, public_port=port, guest_port=mappings.get(i, {}).get('guest_port'),
                  protocol=mappings.get(i, {}).get('protocol'))
             for i, port in enumerate(meta.get('public_ports', []), 1)]
    result = {'GAP_PROJECT_ID': meta['project_id'], 'GAP_VM_ID': meta['vm_id'],
        'GAP_PUBLIC_HOST': hostname, 'GAP_PUBLIC_PORTS': ','.join(str(p['public_port']) for p in ports),
        'GAP_PORTS_JSON': json.dumps(ports, separators=(',', ':')),
        'GAP_HTTP_PORT': str(settings['guest_port']) if enabled else '',
        'GAP_INGRESS_ENABLED': '1' if enabled else '0', 'GAP_PUBLIC_URL': url,
        'GAP_WS_URL': re.sub(r'^http', 'ws', url), 'GAP_BASE_PATH': base_path}
    for i in range(1, 6):
        item = next((p for p in ports if p['slot'] == i), {})
        for key, value in [('PUBLIC', item.get('public_port')), ('GUEST', item.get('guest_port')),
                           ('PROTOCOL', item.get('protocol'))]:
            result[f'GAP_PORT_{i}_{key}'] = str(value) if value is not None else ''
    result['GAP_ENV_REVISION'] = hashlib.sha256(json.dumps(result, sort_keys=True).encode()).hexdigest()
    return result


def validate(values):
    if not isinstance(values, dict) or len(values) > 64:
        raise ValueError('invalid runtime environment')
    for key, value in values.items():
        if (not re.fullmatch(r'GAP_[A-Z0-9_]+', key) or not isinstance(value, str)
                or len(value) > 16384 or any(ord(c) < 32 or ord(c) == 127 for c in value)):
            raise ValueError('invalid runtime variable')
    return values


def install(values, root=Path('/etc/gap'), ssh_dir=None):
    validate(values)
    root.mkdir(parents=True, exist_ok=True, mode=0o755)
    os.chmod(root, 0o755)  # Public metadata must be readable by guest service users.
    files = {'runtime.json': json.dumps(values, sort_keys=True) + '\n',
             'runtime.sh': ''.join('export ' + k + '=' + shlex.quote(v) + '\n' for k, v in sorted(values.items())),
             'runtime.env': ''.join(k + '=' + v + '\n' for k, v in sorted(values.items()))}
    if ssh_dir is not None:
        ssh_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        files[str(ssh_dir / 'environment')] = files['runtime.env']
    for name, data in files.items():
        destination = root / name
        with tempfile.NamedTemporaryFile(mode='w', dir=destination.parent, delete=False) as out:
            out.write(data)
            out.flush()
            os.fchmod(out.fileno(), 0o644)
            os.fsync(out.fileno())
        os.replace(out.name, destination)


def launch(arguments):
    if not arguments:
        raise SystemExit('usage: gap-env COMMAND [ARGUMENTS...]')
    env = os.environ.copy()
    env.update(validate(json.loads(Path('/etc/gap/runtime.json').read_text())))
    os.execvpe(arguments[0], arguments, env)


if __name__ == '__main__':
    install(json.loads(Path(sys.argv[1]).read_text()), ssh_dir=Path('/root/.ssh'))
