# GAP: preapproved Compose in project microVMs

Status: experimental implementation, not deployed on public GAP.
See [operator setup and limitations](../runtime/compose/README.md).

## Agreed architecture

- Operator-preapproved agents on public or private nodes. Compose approval is
  separate from general private-node admission; public registration stays open.
- One exclusive VM/microVM per project; ordinary Docker Engine and Compose
  inside the guest, including guest-root privileges.
- Regular Compose builds, includes, .env, volumes, guest bind mounts, guest
  Docker sockets, privileged containers and guest host networking are allowed.
  All Compose interpretation and external fetches happen inside the guest.
- No GAP commercial CPU/RAM/service quotas or extra GAP egress ACLs for stacks.
  A VM still needs physical RAM/vCPU/disk allocations. Host availability,
  internal-network access and public-IP abuse risks are explicitly accepted.
- No GAT host files, Docker sockets, SSH agents, GAP secrets, KVM devices or
  hypervisor-control sockets are passed into the guest.
- Guest root is not host root. The runner and hypervisor remain trusted,
  patched infrastructure; no zero-risk or unlimited compatibility promise.

This supersedes the earlier restricted rootless-Podman Compose-subset proposal.
Existing GAP KV/objects/functions/sites/realtime quotas are unchanged by this
first implementation; stacks may operate their own databases inside the guest.

## First implementation

Approved owner -> GAP API -> authenticated asynchronous runner job ->
pinned SSH -> exclusive guest -> Docker Compose.

GAP checks live approval and project ownership. The worker maintains its owner/project VM catalog (or checks the legacy
operator inventory with expiry), then calls GAP again before job
admission and execution. Agents never choose the SSH target, key or host command.

The API supports releases, start, stop, status and logs; one stack per project.
A durable journal deduplicates request ids. Per-project operation serialization
avoids concurrent mutations. After controller restart, in-flight jobs become
interrupted rather than being blindly replayed. Guest-side locking handles
overlapping reconnects. None of these controls is commercial billing.

Updates use Compose up, not an atomic blue/green switch. Failed updates can
partially change services. Named volumes persist; release-relative bind paths
change on update. Inspect state before retrying after timeout or disconnection.

## Not delivered yet

- Multi-host scheduling and live migration. The managed worker now builds guest
  assets and creates, starts, stops, resizes offline and destroys QEMU/KVM VMs.
- Application ingress, domain/TLS integration or public host port forwarding.
  Managed VMs have optional worker-loopback TCP forwards.
- HA, migration, automatic rollback and volume backup/restore. Explicit whole-VM
  deletion is supported, with retention by default and data-loss confirmation for purge.
- Automatic VM fencing on revocation. Revocation blocks new management and
  queued execution at recheck; already running apps can continue. The operator
  must fence/stop a VM through the hypervisor for incident containment.
- Removal of existing Cloud service quotas or automatic GAT identity bootstrap.

Enabling Compose does not make a public node private or approve all its agents.
KVM probing and simulated worker tests do not prove real guest boot.

## Acceptance tests and remaining production gate

Automated disposable tests cover KVM boot, Compose build/HTTP, offline resize,
controller reconstruction and deletion. The HTTP integration test also supports
real guests with public/private node approval and persistent volume verification.
Before production activation on a selected execution host:
Test real multi-service builds, persistent volumes, partial updates, controller
restart, SSH failure and revocation. Check no host secrets/control sockets are
present. Test approved, unapproved and cross-project callers before production.

## Primary references

- [Compose trust model](https://docs.docker.com/compose/trust-model/)
- [Docker security](https://docs.docker.com/engine/security/)
- [QEMU microvm guest requirements](https://www.qemu.org/docs/master/system/i386/microvm.html)
- [Firecracker host hardening](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md)

GAP behavior above is our implementation/design, not an upstream guarantee.
