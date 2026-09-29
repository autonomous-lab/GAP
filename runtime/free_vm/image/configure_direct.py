"""Finalize the versioned free image with native, host-filtered networking."""
from pathlib import Path


for name in ('/etc/systemd/system/docker.service.d/proxy.conf',
             '/etc/profile.d/gap-free-proxy.sh',
             '/etc/apt/apt.conf.d/90gap-free-proxy'):
    Path(name).unlink(missing_ok=True)

Path('/app').mkdir(mode=0o755, exist_ok=True)
Path('/etc/profile.d/gap-free-direct.sh').write_text(
    'export PORT="${PORT:-8080}"\n'
)
