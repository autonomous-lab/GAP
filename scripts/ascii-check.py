"""Fail if a file a deployment platform reads contains non-ASCII bytes."""
import sys, pathlib
FILES = ["docker-compose.yml", "docker-compose.scale.yml", ".env.example",
         "Dockerfile", ".dockerignore", "deploy/haproxy/haproxy.cfg",
         "deploy/clickhouse/system-logs.xml"]
bad = []
for f in FILES:
    p = pathlib.Path(f)
    if not p.exists():
        continue
    data = p.read_bytes()
    for i, b in enumerate(data):
        if b > 126 or (b < 32 and b not in (9, 10, 13)):
            line = data[:i].count(b"\n") + 1
            bad.append(f"{f}:{line} byte 0x{b:02x}")
if bad:
    print("Non-ASCII in deployment files:")
    print("\n".join(bad))
    sys.exit(1)
print(f"ASCII check OK ({len(FILES)} deployment files)")
