# MicroVM worker and optional Compose (experimental)

This opt-in worker creates and manages **exclusive QEMU/KVM microVMs per
project**. Agents may run native programs directly; Docker Engine and Compose
are available inside the guest as optional deployment tools. The public node
requires explicit agent approval before use. The worker runs as an unprivileged
Linux user with access to
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

All mutations return asynchronous jobs; poll `/vm/jobs/{job_id}` as for
Compose. Every mutation requires a saved 32-lowercase-hex `request_id`.

| Method and project suffix | Body besides request_id | Behavior |
|---|---|---|
| GET `/vm` | none | Current hypervisor state or `absent` |
| POST `/vm` | optional `vcpus`, `memory_mib`, `disk_gib`, `ports`, `start` | Create exclusive VM; start defaults to true |
| POST `/vm/stop` | `vm_id`, optional `force` | Guest shutdown; explicit force uses verified QMP quit |
| PATCH `/vm` | `vm_id`, selected resource fields or `ports` | Reconfigure a stopped VM; disk growth only |
| POST `/vm/start` | `vm_id` | Start the existing VM |
| DELETE `/vm` | `vm_id`, optional `delete_data`, `confirm_data_loss` | Destroy a stopped VM, retaining its files by default |

Defaults are 1 vCPU, 1024 MiB RAM and 8 GiB virtual disk. The default cumulative quota per approved agent is 2 vCPUs and 4096 MiB RAM. Disk growth is applied to the guest filesystem at next
boot. `ports` contains guest TCP port numbers other than 22; GAP allocates
loopback forwards in the **worker network namespace**, returned as `worker_port`.
Optional ingress publishes this forward under the existing node /apps/ path;
otherwise it remains worker-local.

A returned `running` state describes QEMU, not Docker readiness. Wait for the
guest to boot before submitting a release; a failed early SSH job can be retried
with a new request ID after inspection. Include the returned exact `vm_id` for
subsequent mutations to prevent stale requests from touching a replacement VM.
Deletion rejects running VMs. `delete_data: true` requires
`confirm_data_loss: true` and removes only that generated VM directory; otherwise
files move to the controller's `retained` directory. Automated restore/purge of
retained volumes is not provided. Create does not implicitly deploy a stack;
submit `/stack/releases` once the VM is ready.

## Direct public ports and owner SSH

`runner.example.json` enables `hypervisor.public_network` with hostname
`sites.gap.geta.team` and pool `24000..24099`. `deploy.yml` publishes exactly
that range in TCP and UDP in the shared worker/edge network namespace. This
initial pool can reserve five numbers for up to 20 VMs; it is pool capacity,
not a new per-agent quota. Only configured slots listen. Reserve this range
exclusively for GAP and allow it in provider/host firewalls. The hostname must
resolve directly to the server, without the Cloudflare HTTP proxy. Keep the
private worker RPC and Caddy admin socket private.

To expand capacity, coordinate the configured pool, published Docker range and
firewall; existing allocations must remain in the range. Infrastructure range
changes require redeploying the worker/edge and stop its VMs; ordinary mapping
and SSH-key changes use the API at runtime and require no restart. The controller
uses a durable catalog and a global allocation lock, reserving both protocols
for each of the five numbers. Stop keeps reservations, destruction releases them.
Port exhaustion fails VM creation before disks/processes are created.

VMs created before public networking was configured can acquire their five
slots with their first PUT `/vm/ports`. Owner-key rotation and SFTP require
the current guest helper/image; never replace an image backing existing VM
overlays. Deploy a separate worker/image version if older guests exist.

On a host with a restrictive `DOCKER-USER` chain, run as operator:

```bash
python3 scripts/compose-ports-firewall.py --public-ip YOUR_DIRECT_IPV4
```

This idempotently accepts only TCP/UDP connections DNATed from the specified
public IPv4 and reserved range. It does not flush firewall rules or expose the
RPC port. To persist across boots and Docker restarts, install
`runtime/compose/gap-compose-ports.service` in `/etc/systemd/system/`, set
`GAP_PUBLIC_IPV4=YOUR_DIRECT_IPV4` in `/etc/gap-compose-ports.env`, then run
`systemctl daemon-reload && systemctl enable --now gap-compose-ports.service`.
Adapt the checkout path in the unit for other installations. Reapply after an
external firewall tool replaces the Docker user chain. The default deployment
provides IPv4 access; IPv6 publication is not configured.

The controller uses typed `hostfwd_add`/`hostfwd_remove` via identity-checked QMP.
Agents cannot choose a host address, public port or arbitrary monitor command.
Duplicate UDP guest targets are rejected because libslirp cannot reliably
demultiplex replies across multiple forwards to the same guest UDP port.
No host network-admin capability or host Docker socket is needed. A failed
apply keeps a durable `pending` flag; inspect and reapply with a fresh request
ID. Stopping then starting also rebuilds the desired forwards. Removal closes
listeners but may retain established sessions until VM stop. Raw services must
provide their own authentication/encryption. Guest root remains unrestricted.

The normal agent bearer authorizes GET/PUT `/vm/ports` and `/vm/ssh`.
`python3 scripts/microvm.py --help` lists the agent commands; operator approval
continues to use `scripts/microvm-access.py grant|revoke|list` without reboot.
See [the complete agent guide](../../AGENTS.md#direct-ssh-and-five-public-tcpudp-ports).

## Guest runtime environment

The image installs `environment.py`, `gap-env`, a login profile hook and
`PermitUserEnvironment GAP_*` for root SSH. The guest seed includes a generated
`runtime.json` with only public networking metadata. The boot service installs
it under `/etc/gap/` before sshd and Docker; the root SSH environment file is
managed by GAP. Port/ingress mutations refresh the guest via the restricted
helper, and managed Compose commands synchronize the snapshot before execution.
The helper loads only validated `GAP_*` variables, never the host environment.

`runtime.json`, `runtime.sh` and `runtime.env` are individually replaced
atomically. `GAP_ENV_REVISION` identifies their content; use a single format per
reader. Running process environments cannot be changed externally. Use
`gap-env` for new processes and recreate Compose containers after changes, or
have the app reread the JSON file. Mapping-job failure can mean routing changed
but metadata could not be delivered; `/vm` reports
`environment_sync_pending` for guest update failures. Retry once SSH is ready.

This requires the updated guest image/helper. Do not replace a backing image
under existing overlays: older VMs require a separate image/worker migration.
See [the app integration guide](../../AGENTS.md#runtime-environment-inside-the-microvm).

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
per-container quotas or egress ACLs; agent CPU/RAM quotas still apply. Existing routing/firewalls still apply, and
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
operator-owned microVM approval file, initially `{"agents":[]}`; never put it
in an agent-writable project directory. MicroVM hosting without this file fails startup;
an empty, missing or corrupt list never grants access. Public nodes allow normal
identity/project creation without microVM approval. Approve the owner's exact
DID for microVM access by atomically replacing `/data/compose-agents.json`:

```json
{"agents":["did:gap:<64-hex-character-identity>"]}
```

### Authorize agents without restarting

The environment enables the microVM infrastructure once. Individual permissions
are stored in the operator-owned approval file, reloaded on every request and
before job execution. Use this host command from the repository root:

```bash
python3 scripts/microvm-access.py list
python3 scripts/microvm-access.py grant did:gap:<64-lowercase-hex-identity>
python3 scripts/microvm-access.py revoke did:gap:<64-lowercase-hex-identity>
```

The default host file is `data/gap-node/compose-agents.json`, mounted in the node
as `/data/compose-agents.json`. Use `--file /absolute/host/path` before the action
if the node uses another path. Commands preserve other approvals, serialize
concurrent changes and atomically replace the file; repeat grants/revokes are
idempotent. Run as the operator with write access to this directory. A new file
is mode 600, so ensure the node's OS user can read it.

Authorization applies to the exact agent DID, across that agent's projects.
It neither creates a VM nor publishes an application. Revoking stops new
management/jobs at their authorization check; it does not stop already running
applications. No environment edit or stack reboot is needed for grant/revoke.
The initial worker setup/node transport configuration is a one-time deployment.
The worker Compose file uses its own `gap-compose` project so the node pipeline
cannot remove it as an orphan during node updates.

For a private node, additionally set `GAP_PRIVATE_NODE=1` and
`GAP_PRIVATE_APPROVALS_FILE=/data/private-agents.json`. This second list controls
general Cloud management, not microVM privileges. An agent needs both approvals
to use microVMs in private mode. Private identity creation requires the operator:

```bash
curl -sX POST "$NODE/v1/identity" -H "Authorization: Bearer $ADMIN_TOKEN"
```

Approve the exact returned DID by atomically replacing the approval file:

```json
{"agents":["did:gap:<64-hex-character-identity>"]}
```

The approved agent can now create a project with its own bearer. The file is
reloaded on management authentication; missing/corrupt files deny access. There
is no self-approval endpoint. MicroVM hosting is supported in either node mode.
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
and calls GAP to recheck the active project, owner and microVM approval (plus
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
build contexts inside the guest or upload files through SSH/SFTP. Large-artifact
uploads through the Compose JSON API are not implemented.

Revocation blocks new management and queued execution at recheck, **not already
running VMs/apps or visitor/scoped tokens**. For incident containment, the
operator must fence/stop the VM through the hypervisor first. Custom customer
domains, HA, backup/restore and rollback are not implemented. Each VM supports
five public port slots with TCP/UDP forwarding, configured independently of
Compose. Shared-origin application path routing is available below. Guest
Compose `ports:` does not automatically publish physical-host ports. Existing Cloud service quotas stay unchanged.

## Live per-agent allocation quotas

Each approved agent has a cumulative allocation quota of **2 vCPUs and 4096 MiB
RAM by default**, shared across all its projects. Running, stopped and partially
created VMs count; destroying a VM releases its CPU/RAM allocation. Disk capacity
has no agent quota in this version. Allocate the minimum your workload needs
(default VM: 1 vCPU, 1024 MiB RAM, 8 GiB disk), measure usage, then resize only
when necessary. Do not reserve the full quota for every project.

`GET /v1/cloud/projects/{project}/vm` includes `agent_quota.limits` and
`agent_quota.allocated`, even when that project has no VM. Creation, growth and
start check the live quota; jobs report `agent_quota_exceeded_vcpus` or
`agent_quota_exceeded_memory_mib` when blocked. Lowering a quota does not kill
existing workloads; reductions, stop and destroy remain available. CPU/RAM
quotas are allocations per agent, not a host-wide capacity reservation.

CPU/RAM resize and disk growth require **stop → PATCH /vm → start**. There is no
hot resource resize. Public port mappings and SSH keys can change while running.

On the operator host, use `python3 scripts/microvm-access.py set-quota <DID>
--vcpus 2 --memory-mib 4096` (one shell command). Either flag may be omitted to
preserve that limit. `grant` accepts the same flags. `list` shows effective quotas.
The optional `quotas` map in the approval JSON is keyed by an approved DID and
contains `{ "vcpus": 2, "memory_mib": 4096 }`. Legacy agents-only stores receive
the defaults automatically. Granting again preserves overrides; revoking removes
the override. Changes use the existing atomic approval store and require no restart.
The worker obtains the current limits through the authenticated node callback
and serializes resource mutations across projects belonging to the same owner.
Malformed or unavailable quota policy fails closed. Deploy the updated node
before updating the worker; old nodes do not return the required quota callback.

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
load capacity. Production microVM activation remains a separate operator step.

## Application paths on the existing GAP origin

Applications use `https://gap.geta.team/apps/{project_id}/`, through the node's
existing DNS, TLS certificate and public edge. **No new DNS record, hostname or
certificate is required.** The internal microVM HTTP gateway listens on the private
bridge at port 8093; the existing GAP edge forwards `/apps/` to it. All other
Cloud/function/realtime routes retain their current handling.

The worker assigns each project's path and resolves its selected guest port to
the correct managed microVM. Agents cannot supply upstream IPs or host paths.
Only an explicitly enabled, running VM with that forwarded port is routed.

Operator configuration (also in `runner.example.json`):

```json
"ingress": {
  "dedicated_caddy": true,
  "public_url": "https://gap.geta.team",
  "admin_socket": "/run/caddy-admin/admin.sock"
}
```

`public_url` is the **existing node origin**, not a new app domain; it defaults
to `https://gap.geta.team`. For another node, use that node's existing origin.
The gateway runs plain HTTP internally and does not request TLS certificates.
Its separate Caddy instance owns only application paths. Its admin API uses a
Unix socket shared with the worker, not TCP: guest networking can reach worker
loopback services. Never point it at the shared production edge's admin socket.

### Publish, inspect and disable

First create the VM with the guest port in `ports`, then deploy the application.
Publish that port through the authenticated project API:

```bash
curl -sX PUT "$NODE/v1/cloud/projects/$PROJECT/vm/ingress" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"request_id":"0123456789abcdef0123456789abcdef","vm_id":"vm_<32-hex-id>","enabled":true,"guest_port":8000}'

curl -s "$NODE/v1/cloud/projects/$PROJECT/vm/ingress" \
  -H "Authorization: Bearer $TOKEN"
# url: https://gap.geta.team/apps/prj_<24-hex-id>/
# base_path: /apps/prj_<24-hex-id>/
```

PUT returns a job with the usual request-id retry/deduplication semantics. GET
returns `enabled`, `routed`, `vm_id`, `guest_port`, `base_path` and `url`.
`enabled` is saved intent; `routed` describes the last accepted routing config,
not application health. Disable with a new request ID, the VM ID and
`enabled: false`, omitting `guest_port`.

Publishing is explicit and public. Visitor authentication belongs to the app;
MicroVM approval governs management, not visitors. VM stop/delete withdraws
the route; restarting the same VM restores it. A replacement VM does not inherit
publication. Before VM mutations the worker withdraws the old route, preventing
routing to a reassigned port. A gateway configuration failure blocks that
mutation. Inspect and retry a failed job with a new request ID.

### Application contract

`/apps/{project_id}/api/items?x=1` reaches the guest as `/api/items?x=1`.
Methods, bodies and query strings are forwarded; WebSocket upgrades and streamed
responses are supported. The gateway sets `X-Forwarded-Prefix` to
`/apps/{project_id}` and `X-Forwarded-Proto` from the configured public origin.
The path without its trailing slash redirects to the canonical slash form.
Unknown or disabled project routes return 404.

Applications must support a configurable public base path or generate relative
URLs. Root-relative URLs such as `/assets/app.js`, redirects such as `/login`,
and cookies scoped to `/` are not automatically rewritten. Configure the app's
base URL and cookie path to the returned `base_path`; HTML/JavaScript content
is not rewritten. This is path routing, not a transparent virtual hostname.

All applications share the node's browser origin, like the existing path-based
endpoints. Do not store owner bearers in browser storage. The gateway removes
`Service-Worker-Allowed` responses so guest apps cannot broaden a worker scope
beyond its normal script path. Approval revocation does not stop already running
apps or remove their visitor routes; explicit VM containment remains necessary.

### Internal deployment

Use `runtime/compose/deploy.yml` alongside the node stack. It exposes only
`172.17.0.1:8093` (application gateway) and `172.17.0.1:8092` (worker RPC), not
new public 80/443 listeners. The edge and non-root worker share a network
namespace; guest forwards remain internal to it.

1. Create `data/gap-compose/{config,worker,caddy-data,caddy-config,guest-image,admin}`.
   Writable directories belong to UID/GID 10001; protect `admin` with mode 700.
2. Build guest assets into `data/gap-compose/guest-image` with the earlier build
   command. Directory mode 755 and files 644 allow the read-only image mount.
3. Copy `runtime/compose/runner.example.json` to
   `data/gap-compose/config/runner.json`. Set `node_url` and the existing
   `public_url`; put the matching node/worker secret in `config/service.token`.
   Config directory ownership is 10001/mode 700; files are mode 600.
4. Set `GAP_COMPOSE_KVM_GID` to `stat -c %g /dev/kvm`, then configure Compose
   approval and worker settings on the node as described earlier.
5. From the repository root:

```bash
docker compose --project-directory . -f runtime/compose/deploy.yml config
docker compose --project-directory . -f runtime/compose/deploy.yml up -d --build
```

Rebuild the existing `gap-edge` with the updated `runtime/edge/nginx.conf` to
activate `/apps/` forwarding. Its TLS terminator and DNS remain unchanged.
For a worker on another machine, adapt the edge's private gateway address and
secure that internal transport. The example uses the same execution host.
If recreating the gateway changes its network namespace, recreate the worker
with it. Caddy resumes persisted routing and the worker reconciles catalog state
on startup. No host Docker socket or node secret files are mounted into guests.

### Real routing validation

With `GAP_TEST_CADDY_BINARY` and the real KVM test variables, the integration test
starts a private HTTP gateway. `GAP_TEST_APP_ORIGIN` optionally points to a real
GAP nginx edge in front of it; the default is `http://127.0.0.1:8093`.
`GAP_TEST_APP_PORT` selects the private gateway port (default 8093).
Tests cover GET/POST, preserved queries, assets, slash redirect, WebSocket echo,
disable/re-enable, removal on stop/delete, and restoration after restart with
persistent guest data. This path introduces no new certificate issuance.

## Naming and compatibility

The public machine API is `/vm`; networking lives under `/vm/ports`, `/vm/ssh`
and `/vm/ingress`. `/stack/*` deploys or manages optional Docker containers.
Legacy VM/network routes under `/stack` remain aliases. The operator command
is `scripts/microvm-access.py`; `scripts/compose-access.py` forwards to the same
implementation and approval file. No permission migration is required.
Historical `GAP_COMPOSE_*` configuration variables, the `compose-agents.json`
filename and this runtime directory are infrastructure compatibility names;
they authorize native microVM use too. Granting access does not deploy Docker
containers. The bundled Docker daemon may be stopped for native-only workloads.


## Serverless worker and credit operations

This optional worker feature uses QEMU internal disk snapshots, a loopback HTTP
wake gateway and persistent TCP/UDP listeners. It supports native applications
and optional Compose equally. It does not introduce q35 or hot resource resize.
The API and worker must be upgraded together. The worker compose project is
separate from the main node CI deployment; rebuild/recreate it explicitly.

Before enabling, back up the worker state and stop existing running VMs cleanly.
An unexpected worker restart stops orphaned/unmetered QEMU processes; existing
hibernation snapshots are recovered, while previously running VMs become stopped.
It never replays an old snapshot after a VM has already served writes.

Add these fields to the operator-owned runner JSON (container paths):

```json
{
  "serverless": true,
  "operator_token_file": "/config/billing-admin.token",
  "wake_gateway_port": 8094,
  "host_reserve_memory_mib": 2048,
  "host_reserve_vcpus": 1
}
```

Create `billing-admin.token` with a cryptographically random secret of at least
32 characters, distinct from `service.token`. Restrict it to the worker UID 10001
(mode 0600), mount it with the existing private config directory, and never put
it in Git or the guest. `/operator` requires this separate credential. Keep the
worker RPC private; agents cannot choose prices, recharge themselves, or grant
always-on permission. The wake HTTP port binds loopback in the worker/edge
shared network namespace; do not expose it publicly or bypass Caddy's project
header overwrite. Existing published TCP/UDP slots and DNS remain unchanged.

Grant optional continuous execution without reboot (on the node host):

```bash
python3 scripts/microvm-access.py set-always-on did:gap:<64-hex> --always-on yes
python3 scripts/microvm-access.py set-always-on did:gap:<64-hex> --always-on no
```

The worker starts in **shadow mode with no tariff**, which records raw usage but
neither debits credits nor deletes zero-balance storage. Set real rates only after
choosing the credit conversion, host cost allocation, network/storage costs and
margin. The tariff JSON supports two exact unit schemas: `version`, `vcpu_hour`,
`gib_ram_hour`, plus either legacy `gib_disk_hour`, `gib_in`, `gib_out`, or
commercial `gb_disk_month`, `gb_in`, `gb_out`. Do not mix schemas. Rates are
nonnegative integer microcredits per named unit; enforced mode requires at least
one nonzero rate. Versions are immutable. A commercial disk-month is exactly
730 hours and GB is decimal; RAM remains GiB. Fractional microcredits carry
exactly across checkpoints, worker restarts and tariff changes.

GAP hosted pricing uses **1 credit = USD 1** (1,000,000 microcredits):
**USD 0.010/vCPU-hour**, **USD 0.010/GiB RAM-hour**, **USD 0.10/GB disk-month**,
and **USD 0.01/GB in each network direction**. CPU/RAM bill only while ON;
physical stored data includes hibernation snapshots and remains billable while OFF.
A disk-month means 730 hours, prorated by elapsed time. GB means 1,000,000,000
bytes; GiB means 1,073,741,824 bytes. Conversions and fractional carry are exact.

The approved hosted tariff is `runtime/compose/pricing-usd-v1.json`. It is not
automatically activated on self-hosted workers. After recording authorized
balances, apply it with the operator command below.

```bash
python3 scripts/microvm-billing.py pricing
python3 scripts/microvm-billing.py set-pricing --mode shadow --tariff-file runtime/compose/pricing-usd-v1.json
python3 scripts/microvm-billing.py topup --project "$PROJECT" --owner "$OWNER_DID" --amount-microcredits 10000000 --request-id payment-reference-unique
python3 scripts/microvm-billing.py account --project "$PROJECT" --owner "$OWNER_DID"
python3 scripts/microvm-billing.py set-pricing --mode enforced
```

The top-up amount above is an example, not a production grant. Reuse its request
ID and identical amount after a lost response. Load balances before enforcement:
zero-balance accounts become blocked and enter 72-hour retention when it is enabled.
There is no payment processor integration; the operator records authorized top-ups.
Changing prices checkpoints current usage first and applies the new version to
future intervals. Switching back to shadow pauses automatic unpaid deletion;
a deletion already claimed must complete and cannot be reversed by a mode change.

Back up `microvm-credits.sqlite` using SQLite's online backup API, alongside the
catalog and disks; copying the live main DB alone can omit WAL transactions.
The ledger uses WAL/FULL transactions, atomic checkpoints, integer fractional
carry and unique operation IDs. It retains usage records; monitor ledger growth
and free disk. The API returns the latest 100 entries plus cumulative totals.
Network counters are atomically persisted each second, usage each five seconds
and lifecycle edge. A crash can lose the most recent unflushed counters; it does
not fabricate CPU/RAM time for an unknown outage interval. This is an operational
meter, not a claim of lossless accounting across arbitrary host failures.

Metering observes IP packet sizes at the QEMU network backend, including control
SSH and both incoming/outgoing traffic. A FIFO exposes only packet headers to the
host counter; application payloads are not archived. ARP is not billed. Disk
usage is physical allocated file blocks, excluding the shared read-only base
image; hibernation snapshots and retained disks count. Budget is an execution
threshold and does not waive persistent storage costs. The 72-hour grace period
at zero balance has no hidden debt carried into a later recharge.

Internal TCP source ports are quarantined for 120 seconds of guest execution.
The timer freezes during hibernation and persists with the snapshot. This avoids
reusing a connection tuple while the guest retains old TCP state after slirp
restarts. Source sockets also exclude VM-reserved ports, including hibernated
VMs. Extreme connection churn can temporarily exhaust source ports and returns
an explicit retry error instead of silently colliding with a retained connection.

Inbound application data resets idle time; internal management/health checks do
not. HTTP in flight delays idle hibernation. Silent SSH/TCP/WS sessions can close.
Public scans or client keepalives can keep execution active: expose only necessary
ports and set a budget. Wake checks current approval, cumulative allocation quota,
prepaid balance/budget and host capacity. Always-on permission is refreshed within
30 seconds. Meter or policy failures suspend execution, falling back to a stop if
hibernation fails. Snapshot creation requires available disk space for guest RAM
plus a 512 MiB reserve; keep extra host disk headroom.

Retention cleanup is a durable deletion claim serialized with top-ups, followed
by actual VM and attributable retained-volume removal. A recharge before the
claim cancels expiry; after it, recharge is rejected until deletion finishes.
A worker restart retries incomplete deletion. Do not delete the ledger to reset
credits or copy snapshots between QEMU/image versions.

Validation: `python3 -m unittest discover -s runtime/compose -p 'test_*.py'`.
`serverless_test.py Serverless` is an opt-in real KVM test using the isolated
`integration_test.py` fixture with `GAP_TEST_SERVERLESS=1` and the documented
fixture variables. It exercises public/private owner authorization, protocol
wake, concurrent restore, outgoing-only inactivity, host metering, real test
credits, recharge cancellation and an accelerated 72-hour deletion deadline.


### Multiple managed VMs and billing periods

`GET/POST /v1/cloud/projects/{project}/vms` lists/creates machines in a project.
Read a selected machine using `?vm_id=vm_<32hex>` on `/vm` and its runtime,
ingress, ports or SSH resources. Mutations still identify the VM in JSON.
The legacy default VM and Compose convenience API retain their original paths.
Additional machines use `/apps/{vm_id}/`, separate SSH identities and five
independent public port slots. The default owner quota is still one VM per node;
raise it with `microvm-access.py set-quota --max-vms` when approved.

The WebUI selects, creates and deletes machines. Deletion stops a running VM
gracefully and requires its complete ID; retained disks remain billed. Each
retained generation is metered once, even after creating a replacement. All
project machines share the credit wallet, budget and exhaustion retention.
Usage rows accumulate within VM state/allocation/pricing periods. The five-second
checkpoint and charge transaction remains atomic. Legacy measurements are
archived in `legacy_meter_entries` during migration without changing balances.

Run `collection_integration.py` with `GAP_TEST_VM_COLLECTION=1` alongside the
existing real-KVM test settings in an isolated, disposable container. It checks
two-VM routing, selected wake-up, quota enforcement, deletion/replacement and
project-wide retention expiry. Never point these tests at production state.

### Configure microVM sales prices from a node environment

Set `GAP_PRICING_VERSION`, `GAP_PRICE_VCPU_HOUR_USD`,
`GAP_PRICE_RAM_GIB_HOUR_USD`, `GAP_PRICE_DISK_GB_MONTH_USD`,
`GAP_PRICE_NETWORK_IN_GB_USD` and `GAP_PRICE_NETWORK_OUT_GB_USD` in that
node's `.env`. Values use USD, at most six decimal places; RAM uses GiB,
storage/network use decimal GB, and a storage month is 730 hours.

On the host, preview without credentials or network access, then apply:

```bash
python3 scripts/microvm-billing.py preview-pricing-env
python3 scripts/microvm-billing.py pricing
python3 scripts/microvm-billing.py set-pricing-env --expect-version usd-v1
```

Use `--expect-version none` only to initialize a fresh ledger. For updates,
use the current live version as the expected version and a new version in
`.env` whenever any amount changes. A stale expected version is rejected
atomically. After a lost response, read live pricing before retrying.

Applying prices flushes current VM usage under the old tariff and requires
no stack restart. Editing `.env` or restarting alone does not overwrite live
operator settings. Missing, duplicate, malformed or entirely zero tariffs
are rejected. Unrelated environment secrets are never printed or evaluated.
This configures sales prices; provider costs and financial reports are separate.
