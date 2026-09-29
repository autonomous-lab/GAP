"""Remove free-trial-only network restrictions from the dedicated Debian image."""
from pathlib import Path

Path('/etc/hostname').write_text('gap-guest\n')
Path('/etc/ssh/sshd_config.d/90-gap-free.conf').write_text('''Port 22
HostKey /etc/ssh/ssh_host_ed25519_key
PermitRootLogin prohibit-password
PasswordAuthentication no
KbdInteractiveAuthentication no
AllowAgentForwarding no
AllowTcpForwarding yes
X11Forwarding no
PermitTunnel no
MaxAuthTries 3
MaxSessions 4
MaxStartups 10:30:30
LoginGraceTime 20
PrintMotd no
''')
for name in ('/etc/systemd/system/docker.service.d/proxy.conf',
             '/etc/profile.d/gap-free-proxy.sh',
             '/etc/apt/apt.conf.d/90gap-free-proxy'):
    Path(name).unlink(missing_ok=True)
