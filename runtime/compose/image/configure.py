"""Configure only the disposable guest filesystem during image build."""
from pathlib import Path
import os
import subprocess

root = Path('/guest')
def write(name, value):
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(value)

write('etc/hostname', 'gap-guest\n')
write('etc/fstab', '/dev/vda / ext4 defaults 0 1\n')
write('etc/network/interfaces', 'auto lo\niface lo inet loopback\nauto eth0\niface eth0 inet dhcp\n')
write('etc/modules', 'virtio_net\noverlay\nbr_netfilter\n')
write('etc/ssh/sshd_config', '''Port 22
HostKey /etc/ssh/ssh_host_ed25519_key
PermitRootLogin prohibit-password
PasswordAuthentication no
KbdInteractiveAuthentication no
AllowAgentForwarding no
AllowTcpForwarding no
X11Forwarding no
PermitTunnel no
PrintMotd no
''')
write('etc/docker/daemon.json', '{"log-driver":"local"}\n')
subprocess.run(['chroot', '/guest', '/usr/bin/passwd', '-d', 'root'], check=True)
subprocess.run(['chroot', '/guest', '/usr/sbin/addgroup', '-S', 'docker'], check=True)
for level, services in {
    'sysinit': ['devfs', 'dmesg', 'mdev'],
    'boot': ['modules', 'sysctl', 'hostname', 'bootmisc', 'networking', 'gap-seed'],
    'default': ['sshd', 'docker'],
    'shutdown': ['killprocs', 'savecache', 'mount-ro'],
}.items():
    folder = root / 'etc/runlevels' / level
    folder.mkdir(parents=True, exist_ok=True)
    for service in services:
        path = folder / service
        if not path.exists():
            path.symlink_to('/etc/init.d/' + service)
os.chmod(root / 'etc/init.d/gap-seed', 0o755)
