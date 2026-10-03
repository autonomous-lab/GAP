"""Measure QEMU monitor readiness without starting a guest (node-local diagnostic)."""
import os
import socket
import subprocess
import tempfile
import time
from pathlib import Path


def measure(binary, kernel=False):
    with tempfile.TemporaryDirectory(prefix='gap-qmp-bench-', dir='/data') as directory:
        qmp = Path(directory) / 'qmp.sock'
        command = [binary]
        if binary.endswith('-fast'): command += ['-L', '/usr/share/qemu']
        command += ['-machine', 'microvm,accel=kvm', '-cpu', 'host', '-name', 'gap-bench',
                    '-m', '1024', '-smp', '1', '-nodefaults', '-no-user-config',
                    '-display', 'none', '-serial', 'null', '-qmp', f'unix:{qmp},server=on,wait=off', '-S']
        if kernel:
            command += ['-kernel', '/free-images-v2/vmlinuz', '-initrd', '/free-images-v2/initramfs',
                        '-append', 'console=ttyS0 root=/dev/vda rootfstype=ext4']
        started = time.monotonic()
        with open(os.devnull, 'wb') as devnull:
            process = subprocess.Popen(command, stdin=devnull, stdout=devnull, stderr=devnull)
            try:
                deadline = started + 10
                while time.monotonic() < deadline:
                    if process.poll() is not None: raise RuntimeError(f'QEMU exited: {process.returncode}')
                    if qmp.exists():
                        try:
                            with socket.socket(socket.AF_UNIX) as connection:
                                connection.settimeout(.1)
                                connection.connect(str(qmp))
                                if b'QMP' in connection.recv(512):
                                    return round((time.monotonic() - started) * 1000)
                        except (OSError, TimeoutError): pass
                    time.sleep(.005)
                raise RuntimeError('QMP readiness timed out')
            finally:
                process.terminate()
                process.wait(timeout=10)


for binary in ('/usr/bin/qemu-system-x86_64', '/usr/local/bin/qemu-system-x86_64-fast'):
    for kernel in (False, True):
        print(binary, 'kernel' if kernel else 'bare', [measure(binary, kernel) for _ in range(3)], flush=True)
