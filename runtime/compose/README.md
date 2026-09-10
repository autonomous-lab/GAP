# Preapproved Compose worker (experimental)

This opt-in worker creates and manages **exclusive QEMU/KVM microVMs per
project**, then runs Docker Engine and Compose inside them. It is not enabled
on `gap.geta.team`. The worker runs as an unprivileged Linux user with access to
`/dev/kvm`; it receives no host Docker socket. Legacy operator-provisioned guests
remain supported as an alternative configuration.

## Managed microVM setup

Build the guest assets and worker from the repository root:

```bash
docker build -f runtime/compose/image/Dockerfile --output type=local,dest=guest-image .
chmod 755 guest-image
docker build -f runtime/compose/Dockerfile -t gap-compose-runner:local .
```

The image contains Alpine, a virtio-compatible kernel/initramfs, Docker Engine,
Compose/buildx, OpenSSH and the guest helper. Keep the exported image directory
read-only to the worker. Each VM has its own qcow2 overlay and SSH seed disk;
checksums are verified at creation/start. Do not replace base assets used by
existing overlays: use a separate image directory and worker for a new version.
Build inputs track Alpine package updates; the checksum manifest identifies the
actual exported assets, not a reproducible package lock.

Example managed runner configuration (replace the paths and node address):

```json
{
  "approved_only": true,
  "token_file": "/config/service.token",
  "state_dir": "/data/jobs",
  "node_url": "http://172.17.0.1:8080",
  "hypervisor": {"state_dir": "/data/vm", "image_dir": "/images"}
}
```

Use either `hypervisor` or the legacy `guests` inventory below. The VM state
path must be absolute, contain only letters/digits/underscore/dot/slash/hyphen
and be at most 45 characters (QMP UNIX socket length). Provision writable state
owned by UID 10001 and a token readable only by the worker. Run the image with
`--device /dev/kvm --group-add <kvm-gid>`, the state directory mounted at `/data`,
config at `/config:ro`, and guest assets at `/images:ro`. Do not use privileged
mode or mount the host Docker socket. Bind the worker to a private interface
reachable by GAP; `listen_all: true` is required for an explicit `0.0.0.0` bind.
Do not expose the worker API publicly. Container restarts stop its QEMU children;
the persisted catalog permits explicit VM start after restart.

### VM API

All mutations return asynchronous jobs; poll `/stack/jobs/{job_id}` as for
Compose. Every mutation requires a saved 32-lowercase-hex `request_id`.

| Method and project suffix | Body besides request_id | Behavior |
|---|---|---|
| GET `/stack/vm` | none | Current hypervisor state or `absent` |
| POST `/stack/vm` | optional `vcpus`, `memory_mib`, `disk_gib`, `ports`, `start` | Create exclusive VM; start defaults to true |
| POST `/stack/vm/stop` | `vm_id`, optional `force` | Guest shutdown; explicit force uses verified QMP quit |
| PATCH `/stack/vm` | `vm_id`, selected resource fields or `ports` | Reconfigure a stopped VM; disk growth only |
| POST `/stack/vm/start` | `vm_id` | Start the existing VM |
| DELETE `/stack/vm` | `vm_id`, optional `delete_data`, `confirm_data_loss` | Destroy a stopped VM, retaining its files by default |

Defaults are 1 vCPU, 1024 MiB RAM and 8 GiB virtual disk. These are allocations,
not commercial quotas. Disk growth is applied to the guest filesystem at next
boot. `ports` contains guest TCP port numbers other than 22; GAP allocates
loopback forwards in the **worker network namespace**, returned as `worker_port`.
It does not allocate a public hostname/TLS endpoint. Containerized workers need
an operator-configured proxy in the same network namespace for application access.

A returned `running` state describes QEMU, not Docker readiness. Wait for the
guest to boot before submitting a release; a failed early SSH job can be retried
with a new request ID after inspection. Include the returned exact `vm_id` for
subsequent mutations to prevent stale requests from touching a replacement VM.
Deletion rejects running VMs. `delete_data: true` requires
`confirm_data_loss: true` and removes only that generated VM directory; otherwise
files move to the controller's `retained` directory. Automated restore/purge of
retained volumes is not provided. Create does not implicitly deploy a stack;
submit `/stack/releases` once the VM is ready.

## Legacy guest provisioning (alternative to managed mode)

Run `python3 runtime/compose/preflight.py` on the hypervisor host to check KVM.
This opens an empty VM descriptor, not a running guest. Intel VMX or AMD SVM
must be exposed to Linux through `/dev/kvm`; the API container needs neither.

Provision one separate writable guest disk per project. Install a Linux kernel
with Docker namespaces/cgroups, overlay, networking and the VM's virtio devices;
Docker Engine, Compose v2 supporting `up --wait`, Python 3 and OpenSSH. For QEMU
microvm, provide compatible kernel/initrd assets: ordinary disk firmware boot
is not supported. See [QEMU's boot requirements](https://www.qemu.org/docs/master/system/i386/microvm.html).
The managed image builder above supplies these assets for QEMU microvm.

Install `guest.py` **inside that guest** as:

```text
/usr/local/lib/gap-compose-guest.py
```

Generate a distinct runner SSH key per guest. Only the public key goes into the
guest's root `authorized_keys`; the private key stays with the worker. Suggested
authorized-key restriction:

```text
restrict,command="python3 /usr/local/lib/gap-compose-guest.py" ssh-ed25519 <public-key>
```

Pin the guest's SSH host key from trusted provisioning, not a blind ssh-keyscan.
The known-hosts file uses the inventory VM alias, for example:

```text
project-demo ssh-ed25519 <guest-host-public-key>
```

Never map the inventory to GAT's host SSH service, a shared VM or an ordinary
container. Inventory is operator trust, not VM attestation. Do not pass host
files, control sockets, SSH agents, GAP tokens or KVM devices into guests.
The owner can become guest root and change the helper; its status is not a
security attestation. Keep the hypervisor patched and confined independently.

The operator allocates physical RAM/vCPU/disk. The worker adds no commercial
resource quotas or egress ACLs. Existing routing/firewalls still apply, and
unrestricted access can reach internal services or saturate the host.

## GAP configuration: public or private

Public-node example (ordinary identities/Cloud services remain public):

```dotenv
GAP_PRIVATE_NODE=0
GAP_COMPOSE_ENABLED=1
GAP_COMPOSE_APPROVALS_FILE=/data/compose-agents.json
GAP_COMPOSE_RUNNER_URL=http://172.17.0.1:8092
GAP_COMPOSE_RUNNER_TOKEN=<random-secret-at-least-32-characters>
GAP_ADMIN_TOKEN=<different-random-secret-at-least-32-characters>
```

Generate secrets independently with `openssl rand -hex 32`. The current Compose
stack passes `.env` to GAP and mounts `./data/gap-node` as `/data`. Provision an
operator-owned Compose approval file, initially `{"agents":[]}`; never put it
in an agent-writable project directory. Compose without this file fails startup;
an empty, missing or corrupt list never grants access. Public nodes allow normal
identity/project creation without Compose approval. Approve the owner's exact
DID for Compose by atomically replacing `/data/compose-agents.json`:

```json
{"agents":["did:gap:<64-hex-character-identity>"]}
```

For a private node, additionally set `GAP_PRIVATE_NODE=1` and
`GAP_PRIVATE_APPROVALS_FILE=/data/private-agents.json`. This second list controls
general Cloud management, not Compose privileges. An agent needs both approvals
to use Compose in private mode. Private identity creation requires the operator:

```bash
curl -sX POST "$NODE/v1/identity" -H "Authorization: Bearer $ADMIN_TOKEN"
```

Approve the exact returned DID by atomically replacing the approval file:

```json
{"agents":["did:gap:<64-hex-character-identity>"]}
```

The approved agent can now create a project with its own bearer. The file is
reloaded on management authentication; missing/corrupt files deny access. There
is no self-approval endpoint. Compose is supported in either node mode.
Private management admission does not replace visitor authentication: existing
site/public-function/scoped-WS access rules remain separate. Do not accidentally
expose the private instance. Existing Cloud service quotas are unchanged.

## Worker configuration

Use a dedicated unprivileged host OS user, separate from GAT and its workloads.
Protect its directory/config/journal/keys/token using OS ownership, directory
0700 and secret files 0600. It must not access GAT's Docker socket or secrets.
The token file contains the same `GAP_COMPOSE_RUNNER_TOKEN` configured in GAP.

Example config; replace placeholders and expiry before use:

```json
{
  "approved_only": true,
  "token_file": "/var/lib/gap-compose-runner/service.token",
  "state_dir": "/var/lib/gap-compose-runner/state",
  "node_url": "http://172.17.0.1:8080",
  "guests": {
    "prj_<24-lowercase-hex-characters>": {
      "microvm": true,
      "vm_id": "project-demo",
      "owner_did": "did:gap:<64-lowercase-hex-characters>",
      "expires_at": 0,
      "address": "127.0.0.1",
      "port": 22001,
      "ssh_key": "/var/lib/gap-compose-runner/keys/project-demo",
      "known_hosts": "/var/lib/gap-compose-runner/keys/project-demo.known_hosts"
    }
  }
}
```

`expires_at` is a Unix timestamp; **0 denies access**, not unlimited approval.
The example assumes operator-configured loopback SSH forwarding to the guest;
a routed guest IP also works. Do not share VM aliases, endpoints or private
keys across projects. Do not recycle a VM/disk to another owner without an
explicit data-disposal procedure. Replace inventory atomically to change it.

```bash
python3 runtime/compose/runner.py \
  --config /var/lib/gap-compose-runner/config.json \
  --bind 172.17.0.1 --port 8092
```

Supervise exactly one worker process via the host service manager. Bind to a
private interface reachable from GAP, never expose it publicly; use HTTPS if
control traffic crosses an untrusted network. Keep the node reachable at
`node_url`. Restrict `/internal/` at the external edge where possible; the
callback also requires the shared worker secret. No owner bearer goes to guests.
Service-token rotation requires coordinated worker/node restarts.

GAP forwards scoped jobs outside its global lock. The worker checks inventory
and calls GAP to recheck the active project, owner and Compose approval (plus
general node approval in private mode) before
admission and execution. Agents cannot choose an IP, key or host command.

## Operations and failure semantics

See [AGENTS.md](../../AGENTS.md#compose--experimental) for all API examples.
One stack per project: deploy/update, start, stop, status, logs and job polling.
Ordinary Compose builds, includes, .env, privileged, guest bind mounts and guest
Docker sockets are accepted. Compose is parsed and executed only in the guest.

All POSTs require a 32-lowercase-hex `request_id`. Retry the same body/id after a
lost response to obtain the same job. Changed input with the same id returns
409. Concurrent operations in a project return 409 to avoid conflicting changes.
The journal is durable. Finished jobs clear their input payload, but SQLite
pages/backups may retain secrets; protect the journal accordingly. Guest logs
are returned only to the authorized owner and can contain application secrets.

After worker restart, in-flight jobs become `interrupted`, never blindly replayed.
Inspect status before retrying with a new id. An already attempted guest release
is not replayed. This is not exactly-once execution of arbitrary migrations.
Updates may cause downtime or partial changes. A timeout/disconnection can leave
remote work running; it is not rollback. Guest-side locking serializes operations.
The most recently attempted update remains inspectable/stoppable.

Named volumes survive stop/start/update. Release-relative bind mounts switch to
the next release's directory; use named volumes for persistent app data unless
that behavior is intended. Explicit whole-VM data deletion is available; individual volume/release cleanup is not automated.

HTTP bodies are bounded to 5 MiB including base64/JSON (GAP/proxies can impose
less). Returned output is bounded. Operational timeouts are 540s per guest
command and 600s per SSH session, with a 120s health wait. They are not application
runtime or commercial resource quotas: started apps keep running. Fetch larger
build contexts inside the guest; large-artifact uploads are not implemented.

Revocation blocks new management and queued execution at recheck, **not already
running VMs/apps or visitor/scoped tokens**. For incident containment, the
operator must fence/stop the VM through the hypervisor first. Automatic app
ingress/TLS, public host port forwarding, HA, backup/restore and rollback are
not implemented. Guest `ports:` does not automatically
publish physical-host ports. Existing Cloud service quotas stay unchanged.

## Tests and production readiness

```bash
python3 -m unittest discover -s runtime/compose -v
cargo test --lib
python3 scripts/deploy-check.py
GAP_TEST_BINARY=target/debug/gap python3 runtime/compose/integration_test.py
```

Tests simulate guest execution to check admission, idempotency, serialization,
revocation, bundle paths and command boundaries. They do not prove guest boot,
Docker compatibility or hypervisor isolation. Real end-to-end testing on the
selected execution host is required before calling this production-ready.
The integration test requires `cargo build --bin gap` first. It starts a fresh
GAP nodes in both public and private modes and a real HTTP worker against temporary SQLite stores, but
simulates guest execution. It never connects to production or starts a VM.


### Real KVM acceptance tests

Run only on an execution host prepared for disposable test VMs, using a separate
state directory writable by the test user and the read-only exported image.
The test creates fresh identities/VMs and deletes only those test disks in cleanup:

```bash
GAP_VM_TEST_ALLOW_CREATE=1 GAP_VM_TEST_STATE_DIR=/data GAP_VM_TEST_IMAGE_DIR=/images \
  python3 runtime/compose/real_vm_test.py
GAP_VM_TEST_ALLOW_CREATE=1 GAP_VM_TEST_STATE_DIR=/data GAP_VM_TEST_IMAGE_DIR=/images \
  GAP_TEST_BINARY=/test-target/debug/gap python3 runtime/compose/integration_test.py
```

The first checks actual KVM boot, Docker build/HTTP, stop/resize, controller
reconstruction, restart and deletion. With VM opt-in, the second exercises real
GAP and worker HTTP in both public/private modes, approval/ownership, request
retry deduplication, VM CRUD, a Compose build, HTTP access and a random named-volume
value surviving stop/resize/restart. Without opt-in it keeps simulated guests.
These tests do not establish HA, adversarial hypervisor isolation or production
load capacity. Production Compose activation remains a separate operator step.
