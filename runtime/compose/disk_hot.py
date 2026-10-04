"""Grow the root ext4 filesystem through a VM-scoped, command-restricted SSH key."""
import base64
import json
from pathlib import Path
import subprocess


GIB = 1024 ** 3
GROW_SCRIPT = '''import json, subprocess
device = int(subprocess.check_output(["blockdev", "--getsize64", "/dev/vda"], timeout=10))
subprocess.run(["resize2fs", "/dev/vda"], check=True, timeout=120,
               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
report = subprocess.check_output(["tune2fs", "-l", "/dev/vda"],
                                 stderr=subprocess.DEVNULL, timeout=10, text=True)
fields = dict(line.split(":", 1) for line in report.splitlines() if ":" in line)
filesystem = int(fields["Block count"].strip()) * int(fields["Block size"].strip())
print(json.dumps({"device_bytes": device, "filesystem_bytes": filesystem}))
'''


def key_line(folder):
    public = Path(folder) / 'disk_resize_key.pub'
    if not public.exists():
        return ''
    script = base64.b64encode(GROW_SCRIPT.encode()).decode()
    command = "/usr/bin/python3 -c 'import base64;exec(base64.b64decode(\\\"" + script + "\\\"))'"
    return 'restrict,command="' + command + '" ' + public.read_text().strip() + '\n'


def ensure_key(folder):
    from microvm import VMError
    private = Path(folder) / 'disk_resize_key'
    public = Path(folder) / 'disk_resize_key.pub'
    if private.exists() != public.exists():
        raise VMError('disk_resize_key_incomplete')
    if not private.exists():
        result = subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(private)],
                                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, timeout=15)
        if result.returncode or not private.is_file() or not public.is_file():
            raise VMError('disk_resize_key_creation_failed')


def grow_guest(meta, folder, target_gib):
    from microvm import VMError
    folder = Path(folder)
    command = ['ssh', '-F', '/dev/null', '-T', '-o', 'BatchMode=yes',
               '-o', 'StrictHostKeyChecking=yes', '-o', 'IdentitiesOnly=yes',
               '-o', 'IdentityAgent=none', '-o', 'ForwardAgent=no',
               '-o', 'ForwardX11=no', '-o', 'ClearAllForwardings=yes',
               '-o', 'PermitLocalCommand=no', '-o', 'ConnectTimeout=10',
               '-o', 'GlobalKnownHostsFile=/dev/null',
               '-o', 'UserKnownHostsFile=' + str(folder / 'known_hosts'),
               '-o', 'HostKeyAlias=' + meta['vm_id'], '-i', str(folder / 'disk_resize_key'),
               '-p', str(meta['ssh_port']), 'root@127.0.0.1', 'grow']
    try:
        result = subprocess.run(command, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=150)
        if result.returncode or len(result.stdout) > 4096:
            raise ValueError()
        report = json.loads(result.stdout)
        target = target_gib * GIB
        if (type(report.get('device_bytes')) is not int or report['device_bytes'] < target
                or type(report.get('filesystem_bytes')) is not int
                or report['filesystem_bytes'] < target - 4 * 1024 ** 2):
            raise ValueError()
    except (OSError, subprocess.TimeoutExpired, ValueError, TypeError, json.JSONDecodeError):
        raise VMError('guest_disk_resize_pending') from None
