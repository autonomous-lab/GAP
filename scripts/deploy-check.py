"""Fail if a file a deployment platform reads contains non-ASCII bytes."""
import sys, pathlib
FILES = ["docker-compose.yml", "docker-compose.scale.yml", ".env.example",
         "Dockerfile", ".dockerignore", "deploy/haproxy/haproxy.cfg",
         "deploy/clickhouse/system-logs.xml", "runtime/sandbox/Dockerfile",
         "runtime/sandbox/server.mjs", "runtime/sandbox/worker.mjs",
         "runtime/realtime/Dockerfile", "runtime/realtime/server.mjs",
         "runtime/realtime/package.json", "runtime/realtime/package-lock.json"]
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

# Encoding is not the only way to ship a file a server refuses to read:
# system-logs.xml carried shell-style '#' comments for months and
# ClickHouse rejected the whole file at line 1 column 1 the first time
# it actually ran.
import xml.etree.ElementTree as ET
for f in [f for f in FILES if f.endswith(".xml")]:
    p = pathlib.Path(f)
    if not p.exists():
        continue
    try:
        ET.parse(f)
    except ET.ParseError as e:
        print(f"{f}: not well-formed XML: {e}")
        sys.exit(1)

# The compose files must parse, and must not reintroduce named volumes
# or fixed container names.
try:
    import yaml
except ImportError:
    yaml = None
if yaml:
    for f in [f for f in FILES if f.endswith((".yml", ".yaml"))]:
        p = pathlib.Path(f)
        if not p.exists():
            continue
        doc = yaml.safe_load(p.read_text())
        if "volumes" in doc:
            print(f"{f}: named volumes are back; use ./data bind mounts")
            sys.exit(1)
        for name, svc in (doc.get("services") or {}).items():
            if "container_name" in svc:
                print(f"{f}: service {name} pins container_name")
                sys.exit(1)
            for port in svc.get("ports", []):
                if not str(port).startswith("172.17.0.1:"):
                    print(f"{f}: service {name} publishes {port} outside the bridge")
                    sys.exit(1)

print(f"Deployment check OK ({len(FILES)} files: ASCII, XML, compose)")
