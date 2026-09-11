# GAP Cloud

**The backend agents use to build, deploy and operate applications.**

One project-scoped HTTP API for persistent data, JavaScript functions, static
sites, custom domains, WebSocket communication and opt-in Docker applications. GAP Cloud handles the
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

- **Docker applications (experimental, preapproved agents)** — full Compose stacks
  in a managed microVM per project. Create, start, stop, resize and destroy VMs;
  deploy builds and persistent volumes; publish at `/apps/{project_id}/` using
  the existing node DNS and TLS certificate. No per-app DNS setup.

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

## Run a Docker application

For an operator-approved agent on a Compose-enabled node:

1. Create the project and its microVM with `POST /stack/vm`.
2. Submit the Compose bundle to `POST /stack/releases`; poll its job.
3. Enable `PUT /stack/ingress` for the guest port and use the returned
   `https://gap.geta.team/apps/{project_id}/` URL.

Paths above are relative to `/v1/cloud/projects/{project_id}`. Configure the
application's base path; root-relative links are not rewritten. Applications
are long-running services, not time-bounded serverless function invocations.
Compose on the public deployment requires explicit operator approval.
See the [complete quickstart](./AGENTS.md#managed-app-quickstart),
[lifecycle API](./AGENTS.md#manage-the-microvm-and-publication) and
[operator setup](./runtime/compose/README.md).

## Development

Experimental: [preapproved Compose hosting](./runtime/compose/README.md),
restricted to operator-preapproved agents and **GAP-managed exclusive
project microVMs**. The API and asynchronous SSH worker support deploy/update,
start/stop, status and logs with request-id deduplication. Docker runs only in
the guest; no GAT host Docker socket is given to GAP or workloads.

Enable on public or private nodes with `GAP_COMPOSE_ENABLED=1` and the mandatory
operator-owned `GAP_COMPOSE_APPROVALS_FILE`. Public registration and ordinary
Cloud services remain open; only listed owners can use Compose. Private nodes
also require the separate general node approval. Classic guest Compose is accepted without additional commercial
resource quotas or GAP egress ACLs; existing Cloud service quotas are unchanged.
GAP creates, starts, stops, resizes and destroys microVMs through `/stack/vm`.
The repository includes the guest-image builder and real KVM/API acceptance tests.
Optional ingress publishes a selected guest HTTP port at `/apps/{project_id}/`
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
