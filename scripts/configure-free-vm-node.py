#!/usr/bin/env python3
"""Enable the free-VM entry point without copying any node secret or state."""

import argparse
import json
import os
from pathlib import Path
import re
import secrets
import tempfile


def replace_atomic(path, content, mode):
    ownership = path.stat()
    descriptor, temporary = tempfile.mkstemp(prefix=path.name + ".new-", dir=path.parent)
    try:
        os.fchmod(descriptor, mode)
        os.fchown(descriptor, ownership.st_uid, ownership.st_gid)
        with os.fdopen(descriptor, "w") as output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--ssh-host", required=True)
    parser.add_argument("--max-active", type=int, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[a-zA-Z0-9.-]{1,253}", args.ssh_host) or not 1 <= args.max_active <= 1024:
        parser.error("invalid SSH host or active capacity")
    root = args.root.resolve()
    image = root / "data/gap-compose/free-guest-image-v2"
    for asset in ("SHA256SUMS", "rootfs.ext4", "vmlinuz", "initramfs"):
        if not (image / asset).is_file():
            parser.error(f"missing free image asset: {asset}")

    env_path = root / ".env"
    lines = env_path.read_text().splitlines(keepends=True)
    existing = {}
    for number, line in enumerate(lines):
        match = re.match(r"^(GAP_FREE_VM_[A-Z_]+)=", line)
        if match:
            if match.group(1) in existing:
                parser.error(f"duplicate setting: {match.group(1)}")
            existing[match.group(1)] = number
    values = {
        "GAP_FREE_VM_ENABLED": "1",
        "GAP_FREE_VM_MAX_ACTIVE": str(args.max_active),
        "GAP_FREE_VM_BIND_ADDRESS": "0.0.0.0",
        "GAP_FREE_VM_SSH_HOST": args.ssh_host,
        "GAP_FREE_VM_ABUSE_KEY": lines[existing["GAP_FREE_VM_ABUSE_KEY"]].split("=", 1)[1].strip()
        if "GAP_FREE_VM_ABUSE_KEY" in existing else secrets.token_hex(32),
    }
    if len(values["GAP_FREE_VM_ABUSE_KEY"]) < 32:
        parser.error("existing abuse-control key is too short")
    for key, value in values.items():
        line = f"{key}={value}\n"
        if key in existing:
            lines[existing[key]] = line
        else:
            lines.append(line)

    config_path = root / "data/gap-compose/config/runner.json"
    config = json.loads(config_path.read_text())
    if not isinstance(config.get("hypervisor"), dict):
        parser.error("managed hypervisor is not configured")
    config["hypervisor"]["free_image_dir"] = "/free-images-v2"
    config["free_vm"] = {"enabled": True, "ssh_port": 2121}
    replace_atomic(config_path, json.dumps(config, indent=2) + "\n", config_path.stat().st_mode & 0o777)
    replace_atomic(env_path, "".join(lines), env_path.stat().st_mode & 0o777)
    print(f"Free VM configured for {args.ssh_host}; restart the node and worker after validation")


if __name__ == "__main__":
    main()
