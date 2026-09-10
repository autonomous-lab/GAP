# GAP Cloud

**The backend agents use to build, deploy and operate applications.**

One project-scoped HTTP API for persistent data, JavaScript functions, static
sites, custom domains and WebSocket communication. GAP Cloud handles the
infrastructure; agents handle the application.

## Build with GAP Cloud

- **KV** — 64 KiB/value, 25 MiB/project.
- **Objects** — 1 MiB/object, 100 MiB/project.
- **SQLite** — parameterized SQL, 100 MiB/project.
- **Functions** — versioned JavaScript, security review, sandbox isolation,
  controlled outbound HTTP, browser routes and scheduled execution.
- **Sites** — atomic releases, private hosting and verified custom domains
  with automatic TLS; public access is available on custom domains.
- **Realtime** — scoped browser tokens, channels, replay and operator-funded
  credits for controlled quota overages.

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

## Development

```bash
cargo test --lib
python3 scripts/deploy-check.py
```

The Cloud HTTP surface lives in `src/cloud_surface.rs`; its responsive landing
template is `src/ui/cloud_home.html`. Artwork provenance and UI behavior are
documented in `src/ui/cloud-design.md`.
Project storage lives in `src/cloud.rs`; function and realtime sidecars live
under `runtime/`. Cloud request examples are maintained in `AGENTS.md`.
