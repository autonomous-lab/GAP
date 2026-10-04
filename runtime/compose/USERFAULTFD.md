# Postcopy and userfaultfd on the GAP fleet

The QEMU 11 fast-snapshot worker uses `/dev/userfaultfd` for postcopy RAM
restore. The device is mounted only by `fast-rust.override.yml`; the base
`deploy.yml` does not include it. Always deploy the worker with both files:

```sh
docker compose --project-directory . --env-file .env \
  -f runtime/compose/deploy.yml -f runtime/compose/fast-rust.override.yml \
  up -d --no-build --no-deps compose-runner
```

Build the base worker image before the fast image. Do not rebuild/recreate a
worker while its VMs are running: they are child processes of that container.

On the current Ubuntu 26.04 fleet, keep `vm.unprivileged_userfaultfd=0`.
Ubuntu lists CVE-2026-68166 as vulnerable/work in progress for its Linux
kernel. Globally setting this sysctl to `1` is not the workaround. QEMU 11
prefers opening `/dev/userfaultfd` and can enable postcopy with the sysctl at
`0`, the worker running as UID 10001 with no capabilities, and Docker's default
seccomp profile. The existing udev rule grants the worker group access to the
device. This limits exposure; it does **not** patch the host kernel. Apply the
Ubuntu kernel fix when available, then review this policy.

For a smoke test inside the worker, start the fast QEMU binary with
`-machine none -nodefaults -display none -qmp stdio -S` and send
`qmp_capabilities`, then `migrate-set-capabilities` enabling `postcopy-ram`.
Both commands must return an empty `return` object. Test an actual VM resume
separately. If postcopy cannot be enabled, `memory_fast.py` retries in preload
mode; that mode can be slow for a 1 GiB snapshot and may time out. Keep the
snapshot retryable if no guest instruction has executed.
