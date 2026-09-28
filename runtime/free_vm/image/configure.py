"""Configure only the disposable Debian guest rootfs during image build."""
from pathlib import Path
import os


def write(path, content, mode=None):
    target=Path(path)
    target.parent.mkdir(parents=True,exist_ok=True)
    target.write_text(content)
    if mode is not None:target.chmod(mode)


write('/etc/hostname','gap-free-vm\n')
write('/etc/fstab','/dev/vda / ext4 defaults 0 1\n')
write('/etc/initramfs-tools/modules','virtio_mmio\nvirtio_blk\nvirtio_net\next4\n')
write('/etc/systemd/network/20-gap.network','[Match]\nName=eth0\n[Network]\nDHCP=yes\n')
write('/etc/ssh/sshd_config.d/90-gap-free.conf','''Port 22
HostKey /etc/ssh/ssh_host_ed25519_key
PermitRootLogin prohibit-password
PasswordAuthentication no
KbdInteractiveAuthentication no
AllowAgentForwarding no
AllowTcpForwarding no
X11Forwarding no
PermitTunnel no
MaxAuthTries 3
MaxSessions 4
MaxStartups 10:30:30
LoginGraceTime 20
PrintMotd no
''')
write('/usr/local/bin/gap-free-seed','''#!/bin/sh
set -eu
mkdir -p /run/gap-seed /root/.ssh /etc/gap
mount -t ext4 -o ro /dev/vdb /run/gap-seed
cp /run/gap-seed/authorized_keys /root/.ssh/authorized_keys
cp /run/gap-seed/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key
cp /run/gap-seed/ssh_host_ed25519_key.pub /etc/ssh/ssh_host_ed25519_key.pub
chmod 700 /root/.ssh
chmod 600 /root/.ssh/authorized_keys /etc/ssh/ssh_host_ed25519_key
if [ -f /run/gap-seed/runtime.json ]; then
    python3 /usr/local/lib/environment.py /run/gap-seed/runtime.json
fi
umount /run/gap-seed
resize2fs /dev/vda
''',0o755)
write('/etc/systemd/system/gap-free-seed.service','''[Unit]
Description=Install per-VM GAP identity
DefaultDependencies=no
After=local-fs.target
Before=ssh.service docker.service
[Service]
Type=oneshot
ExecStart=/usr/local/bin/gap-free-seed
RemainAfterExit=yes
[Install]
WantedBy=multi-user.target
''')
write('/etc/systemd/system/docker.service.d/proxy.conf','''[Service]
Environment="HTTP_PROXY=http://10.0.2.100:3128" "HTTPS_PROXY=http://10.0.2.100:3128" "NO_PROXY=localhost,127.0.0.1,::1"
''')
write('/etc/profile.d/gap-free-proxy.sh','''export HTTP_PROXY=http://10.0.2.100:3128
export HTTPS_PROXY=http://10.0.2.100:3128
export http_proxy="$HTTP_PROXY" https_proxy="$HTTPS_PROXY"
export NO_PROXY=localhost,127.0.0.1,::1
export GOPROXY=https://proxy.golang.org
''')
write('/etc/apt/apt.conf.d/90gap-free-proxy','Acquire::http::Proxy "http://10.0.2.100:3128";\nAcquire::https::Proxy "http://10.0.2.100:3128";\n')
write('/etc/docker/daemon.json','{"log-driver":"local"}\n')
Path('/etc/systemd/system/multi-user.target.wants').mkdir(parents=True,exist_ok=True)
for service in ('gap-free-seed.service','systemd-networkd.service','ssh.service','docker.service'):
    target=Path('/etc/systemd/system/multi-user.target.wants')/service
    if not target.exists():
        source=Path('/etc/systemd/system')/service
        if not source.exists():source=Path('/lib/systemd/system')/service
        target.symlink_to(source)
for target in Path('/etc/ssh').glob('ssh_host_*'):
    target.unlink()
# No password login is reachable. Allow root only via per-VM SSH public keys.
os.system('passwd -d root >/dev/null')
