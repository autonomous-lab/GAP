# GAP Cloud

**The backend agents use to build, deploy and operate applications.**

One project-scoped HTTP API for persistent data, JavaScript functions, static
sites, custom domains, WebSocket communication and opt-in Linux microVMs with SSH and Docker applications. GAP Cloud handles the
infrastructure; agents handle the application.

## Build with GAP Cloud

- **KV** — 64 KiB/value, 25 MiB/project.
- **Objects** — 1 MiB/object, 100 MiB/project.
- **SQLite** — parameterized SQL, 100 MiB/project; up to 1,000 parameters and
  4 MiB of combined parameter values per SQL call (API and function bindings).
- **Functions** — versioned JavaScript, security review, sandbox isolation,
  controlled outbound HTTP, browser routes and scheduled execution.
- **Sites** — 3 MiB per file, 100 MiB across releases; atomic releases,
  CSS/JS bundles without content judgement, private hosting and verified custom domains
  with automatic TLS; public access is available on custom domains.
- **Realtime** — scoped browser tokens, channels, replay and operator-funded
  credits for controlled quota overages.

- **Linux microVMs (experimental, approved agents)** — multiple Linux machines
  per project within the owner quota, with root SSH, persistent disk, five public TCP/UDP ports and
  HTTPS/API/WebSocket publication. Run native binaries or language runtimes.
- **Optional Docker/Compose** — deploy and manage containers inside the same
  microVM when your application needs them.

## Quick start

```bash
export NODE=https://gap.geta.team
curl -sX POST "$NODE/v1/identity"
# Save the returned token securely.
export TOKEN=gat_your_token
curl -sX POST "$NODE/v1/cloud/projects" -H "Authorization: Bearer $TOKEN"
```

Read [AGENTS.md](./AGENTS.md) for every Cloud endpoint, request examples,
resource limits, security constraints, custom-domain DNS/TLS and realtime credits.

Browser clients use scoped tokens. Owner credentials and operator credentials
must never be included in a frontend.

Function execution allows 128 total
capability calls, including at most 32 HTTP calls, within a global 30-second
budget (storage/network waits included, queue wait separate). Completed writes
are not rolled back on timeout or quota exhaustion.

Function security reviews run outside the shared node lock, keeping sites and
API reads responsive during publication. Publications are serialized per node;
concurrent attempts receive `429 publication_busy` and should retry with
exponential backoff and jitter. Security and activation rules are unchanged.

Hosted sites (private paths and custom domains) allow HTTPS/WSS browser
connections, HTTPS/blob media, HTTPS frames and same-origin/blob workers.
Upload player JavaScript locally: inline scripts, external script CDNs and
`eval` remain blocked. CORS, codecs and upstream framing restrictions still
apply; function sandbox egress is unchanged. Basic Auth and CSP do not isolate
localStorage between `/sites/` projects sharing the same origin; use dedicated
custom origins for browser-storage isolation and never store owner bearers there.

## Run a node

Copy `.env.example` to `.env`, configure the persistent node identity,
master key, operator token, sandbox token and realtime signing secret, then:

```bash
python3 scripts/init-runtime-secrets.py
docker compose up -d --build
curl http://172.17.0.1:8080/health
```

The stack contains GAP Cloud, ClickHouse, the function sandbox, realtime and
an HTTP/WebSocket edge proxy. Persistent state lives under `./data`.
Back up that directory and your secrets together using a consistent backup.
Custom-domain TLS deployment is described in [deploy/caddy](./deploy/caddy/README.md).

The server exposes Cloud services and identity creation. The legacy contract
API returns HTTP 410. Automatic contract settlement and legacy delivery
workers are disabled. Existing historical records are preserved; ClickHouse
Cloud startup loads identities, Cloud projections and the audit-chain tail,
without hydrating contract, escrow and marketplace projections.

## Archived contract protocol

The former agent-commerce product is archived. Its documentation is preserved
in [archive/contracts](./archive/contracts/README.md), and the pre-pivot source
is available in Git at commit `20a015e`. Shared Rust code remains in the tree
for compatibility during extraction; it is not an active Cloud HTTP product.
Existing contract records are not deleted or automatically settled.
Archived contract SDKs, specifications and adapters are historical references,
not the current Cloud integration contract.
See the [archive inventory and migration notes](./archive/contracts/STATUS.md).

## Linux microVMs with SSH and public ports

Approved agents can use a full Linux microVM directly, with Compose optional.
Each VM reserves **five public port numbers** on `sites.gap.geta.team`, each
configurable for TCP, UDP or both. Map one to guest port 22 for root SSH and
SFTP/SCP; manage Ed25519 keys and mappings at runtime without restarting the VM
or stack. GAP allocates the numbers and returns the SSH host fingerprint.

HTTPS/API/WebSocket apps stay on `gap.geta.team/apps/{project_id}/` and consume
no public port slots. The direct hostname bypasses Cloudflare for SSH/TCP/UDP.
Ports persist across stop/start and are released at destruction. No per-VM DNS
record is needed. Guests receive `GAP_HTTP_PORT`, public ports/mappings, app
URLs and base path automatically. Metadata refreshes at runtime; new SSH
sessions and `gap-env` commands use current values. Compose supports both
interpolation and an explicit container env file. See the
[runtime environment guide](./AGENTS.md#runtime-environment-inside-the-microvm)
and the [agent guide](./AGENTS.md#direct-ssh-and-five-public-tcpudp-ports),
[CLI](./scripts/microvm.py) and [operator setup](./runtime/compose/README.md).

## Serverless microVMs and prepaid credits

Serverless workers hibernate idle VMs to disk after **15 minutes of incoming
inactivity**, releasing CPU/RAM. HTTP/API/WebSocket, TCP/SSH and UDP traffic can
wake them automatically without per-VM DNS. Outbound traffic is billed but does
not keep a VM awake. Always-on is an additional live permission per agent.

A durable, idempotent ledger tracks allocated CPU/RAM time, physical persistent
disk including snapshots, and host-measured IP bytes in both directions.
Versioned prices use integer microcredits (1 credit = 1,000,000 microcredits).
GAP hosted pricing uses **1 credit = USD 1** (1,000,000 microcredits):
**USD 0.010/vCPU-hour**, **USD 0.010/GiB RAM-hour**, **USD 0.10/GB disk-month**,
and **USD 0.01/GB in each network direction**. CPU/RAM bill only while ON;
physical stored data includes hibernation snapshots and remains billable while OFF.
A disk-month means 730 hours, prorated by elapsed time. GB means 1,000,000,000
bytes; GiB means 1,073,741,824 bytes. Conversions and fractional carry are exact.

Shadow mode records usage before real billing is enabled. The microVM wallet is
separate from Realtime credits. Budget thresholds stop execution; retained disk
continues to incur storage charges. At zero credit, storage is kept **72 hours**,
then deleted. Recharge before the deletion claim cancels expiry.

The [microVM console](/microvms), owner API and CLI expose mode, timeout, budget,
usage, balance and deletion deadline. See the
[agent contract](./AGENTS.md#serverless-execution-credits-and-retention) and
[operator setup](./runtime/compose/README.md#serverless-worker-and-credit-operations).

## Run an application without Docker

Create a microVM through `POST /vm` or `scripts/microvm.py create`, configure
SSH and publication, then upload and execute your program with `gap-env`.
Use Linux tools or OpenRC to manage its process and logs. No Compose release
is needed. Follow the [native quickstart](./AGENTS.md#native-application-quickstart).

The operator grants access with `scripts/microvm-access.py grant <DID>`.
This covers the VM and its optional Compose stack without a restart. Historical
`compose-access.py` commands and `/stack/vm` routes remain compatible aliases.

## Run a Docker application (optional)

For an operator-approved agent on a microVM-enabled node:

1. Create the project and its microVM with `POST /vm`.
2. Submit the Compose bundle to `POST /stack/releases`; poll its job.
3. Enable `PUT /vm/ingress` for the guest port and use the returned
   `https://gap.geta.team/apps/{project_id}/` URL.

Paths above are relative to `/v1/cloud/projects/{project_id}`. Configure the
application's base path; root-relative links are not rewritten. Applications
are long-running services, not time-bounded serverless function invocations.
MicroVM access on the public deployment requires explicit operator approval.
See the [complete quickstart](./AGENTS.md#managed-app-quickstart),
[lifecycle API](./AGENTS.md#manage-the-microvm-and-publication) and
[operator setup](./runtime/compose/README.md).

## Development

Experimental: [microVM hosting with optional Compose](./runtime/compose/README.md),
restricted to operator-preapproved agents and **GAP-managed exclusive
project microVMs**. The API and asynchronous SSH worker support deploy/update,
start/stop, status and logs with request-id deduplication. Docker runs only in
the guest; no GAT host Docker socket is given to GAP or workloads.

Enable on public or private nodes with `GAP_COMPOSE_ENABLED=1` and the mandatory
operator-owned `GAP_COMPOSE_APPROVALS_FILE`. Public registration and ordinary
Cloud services remain open; only listed owners can use microVMs, with or without Compose. Private nodes
also require the separate general node approval. Each agent defaults to **1 VM and 2 vCPUs /
4096 MiB RAM total across its microVMs**, including stopped VMs. Allocate the minimum
needed; the operator can change limits live with `scripts/microvm-access.py set-quota
<DID> --vcpus 2 --memory-mib 4096 --max-vms 1`. No per-agent disk quota or GAP egress ACL is applied;
existing Cloud service quotas are unchanged. CPU/RAM/disk resize requires stopping
and starting the VM.
GAP creates, starts, stops, resizes and destroys microVMs through `/vm`.
The `/microvms` WebUI also creates machines, with resource sizing, an optional
SSH public key and a choice to start immediately or keep stopped. Multiple machines can share a project: select, create or delete them in the console.
The default quota remains one VM per agent on this node, adjustable live by the
operator. Shared customer quotas across nodes are planned.
Billing activity accumulates one row per VM state/allocation/pricing period;
internal metering still runs every five seconds to enforce prepaid credit limits.
The repository includes the guest-image builder and real KVM/API acceptance tests.
Optional ingress publishes a selected guest HTTP port at `/apps/{project_id}/`
for the default VM and `/apps/{vm_id}/` for additional VMs
on the existing node origin, reusing its DNS and TLS certificate. No additional
DNS setup is needed. Rollback and automatic VM fencing on revocation remain
unavailable; activation requires the worker and updated internal edge configuration. See the [current architecture](./docs/private-compose-plan.md)
and [API examples](./AGENTS.md#compose--experimental).

```bash
cargo test --lib
python3 -m unittest discover -s runtime/compose -v
python3 scripts/deploy-check.py
```

The Cloud HTTP surface lives in `src/cloud_surface.rs`; its responsive landing
template is `src/ui/cloud_home.html`. Artwork provenance and UI behavior are
documented in `src/ui/cloud-design.md`.
Project storage lives in `src/cloud.rs`; function and realtime sidecars live
under `runtime/`. Cloud request examples are maintained in `AGENTS.md`.

Operational node inventory and fresh-node recovery: [nodes](docs/nodes.md).

### Email verification and human signup

`/signup` provides email-code verification, an API credential and first-project
creation. Enable it with `GAP_EMAIL_VERIFICATION_REQUIRED=1`, a configured
`GAP_MASTER_KEY`, synchronous ClickHouse inserts (the default), and the local SMTP settings in `.env.example`. On Elestio, run
`python3 scripts/configure-elestio-smtp.py` to read the authorized sender from
`/opt/elestio/startPostfix.sh`; upstream relay credentials stay inside Postfix.
The transport supports a private or loopback IPv4 SMTP endpoint without TLS,
intended only for the local relay. Use Postfix for authenticated upstream TLS.

Challenges live in `/data/registration.sqlite`, survive restarts, expire after
ten minutes, allow five guesses, and cannot be replayed. Codes are stored as
keyed HMAC digests; they are never returned by the API or logged. New credentials
are issued only after successful verification and storage writes. Existing
identities remain usable without being falsely labelled verified. This is a
node-local registration foundation, not shared fleet authentication or admin MFA.
See [agent instructions](AGENTS.md) for endpoints, rate limits and error handling.

MicroVM sales tariffs can be loaded explicitly from each node's `.env` with
`python3 scripts/microvm-billing.py preview-pricing-env` and
`set-pricing-env --expect-version <live-version>`. Live rates are versioned and
are never silently reset by a restart. See [operator instructions](./AGENTS.md#configure-microvm-sales-prices-from-a-node-environment).
