# Private GAP: isolated Compose hosting (proposal)

Status: design only. No Compose deployment endpoint, runner, or private-agent
approval mechanism is implemented by this document. Public GAP must not expose
this feature. Static file changes are independent and already documented in
AGENTS.md.

## Recommended architecture

Accept a deliberately restricted Compose manifest, not arbitrary Docker control.
Use a separate execution worker and one VM/microVM per project/stack. The GAP
API and its ClickHouse, signing keys and existing runtimes stay off that worker.

```text
Preapproved agent -> private GAP API -> policy validation + immutable release
                                       -> authenticated runner job
Dedicated worker -> project VM -> rootless container CLI -> app + db + cache
Ingress gateway -----------------------> approved app service/port only
```

Inside each guest, use rootless Podman through fixed argument arrays (no shell
constructed from input), without starting its API service. Compile supported
Compose fields into a normalized execution plan; do not invoke an arbitrary
Compose provider against raw user YAML. This is Compose-subset compatibility,
not a promise that every existing docker-compose.yml works unchanged.

Neither agents, app containers nor the GAP API receive a Docker/Podman socket,
remote engine API, host shell, KVM device or hypervisor control API. A small
trusted runner on the execution worker necessarily manages guest lifecycles;
it receives only scoped, authenticated jobs, not arbitrary host commands.
Its hypervisor permissions are part of the trusted computing base. No claim
of zero host-compromise risk: patching and boundary testing remain mandatory.

Preflight: check hardware virtualization/nested virtualization, host kernel,
cgroup enforcement, networking, disk quotas and recovery procedures. If KVM is
unavailable on the current GAP VM, use separately provisioned worker VMs; never
silently fall back to privileged Docker-in-Docker or plain host containers.
Rootless containers alone share a kernel and are not the selected tenant boundary.
gVisor is an alternative to evaluate, not an assumed drop-in guarantee for all
images. Keep this workload away from the existing public GAP VM initially.

## Private-node admission and revocation

Proposed switches: `GAP_PRIVATE_NODE=1` and `GAP_COMPOSE_ENABLED=1`; enabling
Compose without private mode fails startup. Empty approval list denies access.
Authentication alone, a Docker network, or possession of a project id never
confers deployment permission.

The operator approves exact agent identities, tied to the local GAT instance,
with `compose.deploy`, allowed projects, expiry and resource ceilings. Import
approval from a verified GAT assertion or provision it manually; do not trust a
caller-supplied instance id, email, forwarded header or source IP. Agents cannot
self-approve. Operator credentials never enter a guest or an agent frontend.

Check approval and project ownership on validate, deploy, update, start,
rollback, status, logs and volume operations. Recheck at execution time, not
just when a job is queued. Jobs bind manifest digest, image digests, policy
version and project, with expiry and replay protection. An update gets the same
validation as a first deployment; rollback must satisfy current policy too.

Revocation cancels queued jobs, blocks new actions, fences ingress/egress and
stops the guest; retain volumes pending operator action rather than deleting
data. Fence on expired runner leases if the control plane is unavailable.
Audit approvals, denials, normalized releases, starts/stops, revocations and
operator overrides. This admission system is a prerequisite, not an optional UI.

## Manifest allowlist, before any external side effect

Version the accepted schema and reject unknown keys. Bound YAML bytes, depth,
node count and aliases; reject duplicate keys and custom tags. Disable YAML
alias expansion in the MVP. Do not resolve includes or environment interpolation
against the runner host. Never run `docker compose config` on untrusted input.

MVP allows: pinned OCI images, command/entrypoint arrays executed inside the
guest, explicit environment values and project secret references, bounded
healthchecks, dependency ordering, project-private networks, named project
volumes, tmpfs and operator-capped resources. Secret files are generated in the
guest from the project secret store, not read from a submitted host path.

Reject: `build`, `include`, `extends`, `env_file`, file-backed configs/secrets,
external volumes/networks, host bind mounts, devices, socket mounts, `privileged`,
`cap_add`, host PID/IPC/network/user namespaces, unconfined security profiles,
custom runtimes, sysctls, custom DNS/extra_hosts, lifecycle host hooks, arbitrary
host port publications and labels that configure the platform proxy. Only
explicitly supported nested fields are allowed, including volume driver options.

Normalize to platform-generated names. Pin every image to a resolved digest,
limit registry access, image size/layers and extraction size. No builds in MVP;
build images elsewhere. Never evaluate registry/image-controlled host hooks.
The runner revalidates the normalized plan before executing it.

## Isolation, networking and limits

Enforce non-root users, dropped capabilities, no-new-privileges, default seccomp
and mandatory access controls where supported. Prefer read-only root filesystems
with explicit writable project volumes/tmpfs; reject incompatible images with
a clear error rather than granting privileges. Verify limits actually work.

Proposed starting quota per approved project (operator configurable, not live):
one running stack, four services, 2 vCPU and 2 GiB RAM aggregate, 256 PIDs per
service and 512 aggregate, 10 GiB persistent storage, bounded temporary/image
storage and rotated logs (100 MiB aggregate). Reserve guest/runner overhead
separately. Admission uses aggregate available worker capacity, with disk
headroom; failed placement never kills an unrelated project to make room.

Internal DNS/service discovery only within a project's guest. Egress is denied
by default at a boundary workloads cannot reconfigure. Broker approved HTTPS
destinations and DNS, with destination-IP validation for IPv4/IPv6, redirects
and DNS rebinding. Deny host, metadata, management networks and other tenants.
Do not rely on an in-app proxy environment variable as the enforcement boundary.
Other outbound protocols require explicit later policy, not unrestricted NAT.

Ingress exposes only an operator-approved HTTP/WebSocket service through GAP's
gateway, with TLS, authentication, rate/body/connection limits and verified custom
domain ownership. Databases remain internal; no raw host ports in MVP. Gateways
must not proxy to agent-supplied arbitrary addresses. Private-node access does
not implicitly authorize publishing an application to the Internet.

## Lifecycle and candidate API

Proposed owner-scoped paths (not available today), all subject to approval:

- `POST /v1/cloud/projects/{id}/stacks/validate`: normalized plan + denials.
- `POST /v1/cloud/projects/{id}/stacks`: immutable release + asynchronous job.
- `POST /v1/cloud/projects/{id}/stacks/{stack}/releases`: update through same gate.
- `GET /v1/cloud/projects/{id}/stacks/{stack}`: state, release and resource usage.
- `GET /v1/cloud/projects/{id}/stacks/{stack}/logs`: bounded authorized logs.
- `POST .../{stack}/start`, `/stop`, `/rollback`: audited lifecycle operations.
- `DELETE .../{stack}`: remove execution, keep volumes by default.

Suggested state machine: validating -> queued -> provisioning -> starting ->
healthy, or failed/stopped/revoked. Persist intent before dispatch; idempotency
keys and reconciliation prevent duplicate stacks after retries/restarts.
Never run deployment work under the global GAP state lock.

MVP updates may use stop/start with explicit downtime. Do not promise atomic
blue/green updates for stateful stacks: two writers sharing a volume can corrupt
data. Snapshot/backup before migration; restoring application images alone does
not reverse database migrations. Volume deletion needs separate confirmation
and a documented recovery policy. Exclude secrets from routine status/log output;
application-controlled logs must still be treated as potentially sensitive.

## Delivery phases and acceptance gates

1. Private-node identity/approval model, revocation, audit and deny-by-default tests.
2. Standalone manifest validator and dry-run API, with no execution privileges.
3. Dedicated worker VM/microVM proof of concept with one approved non-root image.
4. Multiple services, private DNS/volumes, HTTP/WS ingress, quotas and reconciliation.
5. Updates, backup/restore, monitoring, revocation drills and restricted private beta.

Before beta, test rejected socket/device/bind mounts and nested bypasses;
unauthorized/cross-project requests; revoked queued jobs; CPU/fork/disk/log bombs;
SSRF including IPv6/metadata/DNS rebinding; malicious registry archives; crashes
during deploy/update; restored volume consistency; worker reboot; lease expiry;
and absence of host secrets in guest files, process environments and logs.
Prove another tenant and the control plane stay reachable during each test.

## Primary references

- [Compose trust model](https://docs.docker.com/compose/trust-model/): untrusted
  manifests can introduce host access through nested files and referenced paths.
- [Docker rootless mode](https://docs.docker.com/engine/security/rootless/) and
  [resource-control requirements](https://docs.docker.com/engine/security/rootless/tips/).
- [Podman Compose provider](https://docs.podman.io/en/latest/markdown/podman-compose.1.html):
  a Compose command can delegate to an external provider; it is not our validator.
- [Firecracker host hardening](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md)
  and [jailer](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md).
- [gVisor security model](https://gvisor.dev/docs/architecture_guide/security/).

The architecture and quotas above are GAP design proposals, not guarantees made
by these upstream projects and not capabilities deployed on the existing node.
