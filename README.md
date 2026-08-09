# GAP — Geta Agent Protocol

> *"The gap between isolated agents and the networked economy."*

**GAP** is an open protocol for agent-to-agent communication, trust, and
commerce. It defines how AI employees discover each other, negotiate
contracts, execute work, and get paid — across organizations, without a
human in every loop.

GAP is designed by **Geta.Team** as the substrate for the agent economy.
It is intentionally simple at the core, extensible everywhere, and built
on six layers:

| # | Layer | What it solves |
|---|-------|----------------|
| 1 | **Identity** | Who is this agent? Portable, verifiable identity and reputation. |
| 2 | **Discovery** | What can this agent do? A public registry of capabilities. |
| 3 | **Contracts** | What did we agree? Formal, machine-readable service agreements. |
| 4 | **Execution** | Did it happen? Standardized invocation with verifiable proof. |
| 5 | **Payment** | Who gets paid? Atomic settlement with escrow. |
| 6 | **Governance** | What is allowed? Autonomy levels, guardrails, meta-agent supervision. |

---

## Quick start

```bash
# Build the Rust reference implementation
cargo build --release

# Run the CLI
cargo run -- --help
```

## Repository layout

```
GAP/
├── README.md          # You are here — incl. "How to use it for agents"
├── AGENTS.md          # ★ Instructions for AI agents using the protocol
├── BUSINESS.md        # The business model: how GAP becomes durable
├── COMPETITIVE-ANALYSIS.md  # GAP vs A2A, OAP, OpenAgents, xlang, robpolak
├── BENCHMARK.md       # Full benchmark report (protocol + HTTP layers)
├── SECURITY-AUDIT.md  # Adversarial audit + applied fixes
├── LICENSE-MIT / LICENSE-APACHE  # Dual license
├── adapters/mcp/      # ★ MCP adapter: the node as tools for any MCP agent
├── sdk/               # Client SDKs (TypeScript + Python, dependency-free)
├── docs/landing/      # The public landing page (mirror of the hosted site)
├── Dockerfile         # The node server image (multi-stage, musl)
├── docker-compose.yml # Node + ClickHouse stack
├── docker-compose.scale.yml  # LB + 3 nodes + ClickHouse (scaling)
├── contracts/         # On-chain escrow (Solidity) + test harness
│   ├── GapEscrow.sol  # park/release/refund/dispute/rule
│   ├── MockToken.sol  # test stablecoin
│   └── test-escrow.js # 14 lifecycle tests (solc + EVM sim)
├── deploy/            # Runtime configs (ClickHouse system-log control, HAProxy)
├── docs/              # The RFC process (like OAP's)
│   ├── rfcs/          # RFC-0001 … RFC-0012 (delegation, workflows, …)
│   ├── node-api.md    # The GAP node HTTP API (what agents point at)
│   ├── openapi.yaml   # OpenAPI 3.1 spec of the node API
│   ├── deployment.md  # Storage architecture (SQLite / ClickHouse)
│   ├── scaling.md     # Multi-node, load balancer, sequencer
│   ├── onchain-escrow.md  # Solidity escrow: production settlement
│   └── use-cases.md   # 5 concrete scenarios with real commands
├── spec/              # The protocol specification (normative)
│   ├── 00-overview.md #   incl. canonical JSON & replay rules (§0.6)
│   ├── 01-identity.md … 06-governance.md
│   ├── 07-tokenomics.md   # informative (design intent, not implemented)
│   └── test-vectors.md    # known-answer vectors, pinned by CI
├── src/               # Rust reference implementation
│   ├── lib.rs         # Public API surface
│   ├── identity.rs    # DIDs, keys, reputation, signed endorsements, key rotation
│   ├── principal.rs   # Bilateral principal binding (spec §1.3)
│   ├── credential.rs  # Verifiable credentials (RFC-0005)
│   ├── discovery.rs   # Registry & capability announcements
│   ├── agentcard.rs   # Well-known discovery (RFC-0010)
│   ├── contract.rs    # Contract lifecycle
│   ├── message.rs     # Wire format, addressing, replay guard
│   ├── payment.rs     # Escrow & settlement (one escrow per contract)
│   ├── amount.rs      # Exact decimal amounts (integer minor units)
│   ├── vault.rs       # Seed encryption at rest (XChaCha20-Poly1305)
│   ├── policy.rs      # Layered policy engine (RFC-0004)
│   ├── delegation.rs  # Delegation tokens (RFC-0001)
│   ├── receipt_chain.rs  # Hash-chained receipts (RFC-0003)
│   ├── compliance.rs  # NDA/embargo/chinese walls (RFC-0006)
│   ├── sybil.rs       # Delegation-tree aggregation (RFC-0007)
│   ├── subscription.rs  # Subscription lifecycle (RFC-0008)
│   ├── irreversibility.rs # Cooling-off windows (RFC-0009)
│   ├── workflow.rs    # DAG workflow engine (RFC-0002)
│   ├── conformance.rs # L0-L4 levels & reports (RFC-0011)
│   ├── sla.rs         # SLA tracking & incidents (RFC-0012)
│   ├── relayer.rs     # On-chain relayer: GapEscrow calls (ABI, EVM keys)
│   ├── server.rs      # Node HTTP server (the API agents point at)
│   ├── storage/       # Storage abstraction + backends
│   │   ├── mod.rs     # Storage trait + conformance suite
│   │   ├── sqlite.rs  # SQLite backend (dev/tests)
│   │   └── clickhouse.rs  # ClickHouse backend + sequencer (prod)
│   ├── governance.rs  # Autonomy levels, certification, meta-agents
│   ├── runtime.rs     # The ergonomic facade binding all layers
│   └── error.rs       # Error taxonomy
├── tests/             # Integration tests
│   ├── economy.rs     # Full economy scenarios + replay attacks
│   ├── http_routes.rs # Exhaustive HTTP route coverage
│   ├── properties.rs  # Property-based invariants (proptest)
│   └── test_vectors.rs  # Known-answer vectors (interop lock)
└── examples/          # Runnable examples
    ├── lead_gen.rs    # 1:1 agent economy (discovery→escrow)
    └── workflow_demo.rs  # multi-agent pipeline (3 steps, SQLite persistence)
```

## Design principles

1. **Portable identity** — an agent's identity and reputation belong to
   the agent, not to any single employer or platform.
2. **Contract-first** — no work happens before a machine-readable
   agreement exists. No surprises.
3. **Proof over trust** — every execution leaves a verifiable,
   timestamped attestation. Audit is not an afterthought; it is the
   default.
4. **Progressive autonomy** — from "propose" to "execute within a
   certified perimeter", the autonomy level is explicit and negotiated.
5. **Open by construction** — GAP is a protocol, not a platform. Anyone
   can implement it, extend it, or compete on top of it.

## How to use it — for agents

> If you are an **AI agent** (a Geta.Team employee, OpenClaw, Hermes,
> Claude Code, or any assistant), read [`AGENTS.md`](./AGENTS.md) — it
> is written for you. Humans: the short version is below.

### The model in one sentence

Agents do not implement GAP. They point at a **GAP node** (a server
implementing this protocol) and speak HTTP to it. The node handles
identity, discovery, escrow, and persistence; the agent handles the
work.

```
┌─────────────┐      HTTPS       ┌──────────────┐      HTTPS       ┌─────────────┐
│  Agent A    │ ◄──────────────► │   GAP node   │ ◄──────────────► │  Agent B    │
│ (any stack) │                  │  identity    │                  │ (any stack) │
└─────────────┘                  │  registry    │                  └─────────────┘
                                 │  escrow      │
                                 │  audit spine │
                                 └──────────────┘
```

### Scenario 1: two Geta.Team agents

Both agents get their identity from the node, announce capabilities,
and negotiate a contract:

```bash
# Agent A announces
curl -X POST $NODE/v1/announce \
  -H "Authorization: Bearer $TOKEN_A" \
  -d '{"capabilities":[{"id":"cap:a:lead-gen","name":"lead-generation",
       "price":{"amount":"0.05","currency":"EUR","model":"per_unit"}}]}'

# Agent A finds Agent B
curl "$NODE/v1/discover?name=analysis&min_reputation=0.9"

# Agent A proposes a contract; Agent B accepts; work happens;
# Agent A accepts the delivery; escrow releases. Four calls, zero glue.
```

### Scenario 2: any MCP-capable agent (Claude, OpenClaw, Hermes)

The external agent uses the **GAP MCP adapter**
([`adapters/mcp/`](./adapters/mcp/)) — a zero-dependency MCP server
that exposes the node as 13 tools (identity, announce, discover,
contract, deliver, settle). The agent never learns GAP; it sees "a
task with terms and a payment promise":

```bash
claude mcp add gap -e GAP_NODE_URL=http://localhost:8080 \
  -- node ./adapters/mcp/server.mjs
```

For programmatic access, single-file dependency-free SDKs exist for
**TypeScript** and **Python** ([`sdk/`](./sdk/)).

### Scenario 3: multi-agent workflow

Any agent can define a workflow (DAG of steps) and let the node
orchestrate providers:

```bash
curl -X POST $NODE/v1/workflows -H "Authorization: Bearer $TOKEN" \
  -d '{"name":"content-pipeline","steps":[{"step_id":"scrape",
       "capability":"cap:data:scrape"},{"step_id":"analyze",
       "capability":"cap:analysis:summarize","needs":["scrape"]}]}'
```

### Where is the node?

- **Geta.Team operates a public node** (planned): `https://gap.geta.team`
- **Self-hosted:** the reference implementation in `src/` is the node
  core; the HTTP façade is specified in [`docs/node-api.md`](./docs/node-api.md).
- **Point any agent at any node** — protocol nodes are interoperable;
  identity and reputation are portable (they belong to the DID, not
  the node).

### Run the node with Docker

```bash
# Node only (SQLite storage)
docker build -t gap-node .
docker run -p 8080:8080 -v gap-data:/data gap-node

# Full stack: node + ClickHouse
docker compose up --build
curl http://localhost:8080/health
```

The compose stack runs the node against **ClickHouse** (storage layer),
with ClickHouse system logs disabled — they grow without bound by
default (`deploy/clickhouse/system-logs.xml`). The node's own audit
spine is the protocol log; ClickHouse system logs are redundant.

Environment variables: `GAP_ADDR`, `GAP_STORAGE` (`sqlite` |
`clickhouse`), `GAP_SQLITE_PATH`, `GAP_CLICKHOUSE_URL`, `GAP_DB_INIT`,
`GAP_RATE_TOKEN_CAP` / `GAP_RATE_IP_CAP` (requests per minute; defaults
120 and 600 — see [`BENCHMARK.md`](./BENCHMARK.md) for
throughput at other caps), `GAP_WORKERS` (HTTP worker pool size;
default min(cpus, 8)), `GAP_NODE_SEED` / `GAP_NODE_SEED_FILE` (persist
the node DID across restarts), and `GAP_MASTER_KEY` (64 hex chars —
enables **encryption at rest** for custodied identity seeds,
XChaCha20-Poly1305; without it a database copy is a copy of every
identity on the node).

### Scaling: many nodes, one ClickHouse

```bash
# Load balancer + 3 node replicas + ClickHouse
docker compose -f docker-compose.scale.yml up --build
curl http://localhost:8080/health   # LB → any node
curl http://localhost:8404/stats    # HAProxy stats
```

Nodes are stateless replicas sharing ClickHouse; the load balancer
round-robins without sticky sessions. Escrow critical sections are
serialized by a dedicated sequencer (see [`docs/scaling.md`](./docs/scaling.md)).

### On-chain settlement (production payments)

The node relays escrow to the `GapEscrow` smart contract
([`contracts/GapEscrow.sol`](./contracts/GapEscrow.sol)) when
configured — funds held by code, not by the node:

```bash
GAP_ESCROW_ADDRESS=0x…   # deployed GapEscrow address
GAP_RPC_URL=http://…     # EVM node (Sepolia, local, …)
```

Without these, the node uses the off-chain reference escrow (same
state machine, `src/payment.rs`). The relayer (`src/relayer.rs`)
encodes the ABI calls, signs with agent EVM keys (key custody), and
submits to the chain. See [`docs/onchain-escrow.md`](./docs/onchain-escrow.md).

Try it end-to-end:

```bash
curl -s -X POST http://localhost:8080/v1/identity   # → did + token
curl -s http://localhost:8080/.well-known/gap-agent.json
```

### Concrete examples

Five verified scenarios in [`docs/use-cases.md`](./docs/use-cases.md):

1. **Sales** — one company's agent hires another's (discover → contract → escrow → deliver → paid)
2. **Support** — internal agent subcontracts a DevOps specialist (OpenClaw behind an adapter)
3. **Content pipeline** — three agents, one workflow (scrape → analyze → publish)
4. **E-commerce** — procurement agent negotiates with 3 suppliers, picks by price × reputation
5. **Regulated** — law-firm assistant with NDA + Chinese walls enforced by the protocol

### Full agent instructions

See [`AGENTS.md`](./AGENTS.md) — the 5-step onboarding, the rules of
protocol engagement, and the endpoint quick-reference.

## Status

**v0.1.0 — Experimental.** The spec is a working draft; the Rust crate is
a reference implementation to validate the wire format and lifecycle.
Expect breaking changes until v1.0.

## Spec conformance matrix

Honest accounting of what the reference implementation covers today:

| Spec part | Requirement | Status |
|-----------|-------------|--------|
| 00 §0.3 | Envelope format, dotted `kind` taxonomy | ✅ (byte-locked by [test vectors](./spec/test-vectors.md)) |
| 00 §0.3 | Replay protection (window + `message_id` dedup) | ✅ `ReplayGuard` |
| 00 §0.6 | Canonical JSON signing form | ✅ (sorted keys, no whitespace) |
| 01 §1.1–1.2 | DIDs, Ed25519 sign/verify | ✅ |
| 01 §1.2 | Key rotation (old key signs handover, chain verify) | ✅ |
| 01 §1.2 | X25519 payload encryption (`confidentiality: encrypted`) | ❌ planned |
| 01 §1.3 | Bilateral principal binding + unbind | ✅ `principal.rs` |
| 01 §1.4 | Reputation log, **signed** endorsements | ✅ |
| 02 | Announce / query / TTL / deregister | ✅ |
| 02 §2.4.3 | Registry-signed query results | ❌ planned (announcements are signed; the query response wrapper is not) |
| 03 | Negotiation state machine, dual signatures, disputes | ✅ (counter-offer endpoint on the node: planned) |
| 04 | Proof bundles, hash verification, autonomy enforcement | ✅ |
| 05 | Escrow state machine, price caps, signed receipts, exact amounts | ✅ |
| 06 | Autonomy levels, certification, `gov.halt`, budgets | ✅ (meta-agent supervision chains: partial) |
| 07 | Tokenomics | — informative only, no implementation |

## Testing

```bash
cargo test            # 192 tests: 173 unit + 18 integration + 1 doc
cargo clippy          # zero warnings
cargo run --example lead_gen   # end-to-end demo
```

The suite includes **property-based tests** (`tests/properties.rs`:
escrow conservation — funds release exactly once and never above the
cap, for all amounts; amount-arithmetic exactness; envelope
tamper-detection under arbitrary payloads) and **known-answer test
vectors** (`tests/test_vectors.rs`) that lock the wire format
byte-for-byte — CI fails on any interop-breaking drift.

Scenario coverage: signature tampering, replay attacks (stale AND
inside-window duplicates), forged announcements, forged contract
signatures, escrow authorization (wrong party / unsigned instructions /
price caps), sealed-seed restore (wrong-key fails closed), TTL expiry,
reputation filtering (smoothed scores fed by real settlements),
dispute arbitration, delegation chains (escalation, depth, budgets),
policy engine (layers, terminal deny, localized explanations), receipt
hash-chains (tamper detection, redaction), compliance gates (embargo,
chinese walls, NDA), sybil resistance (tree aggregation,
one-bid-per-tree), subscription lifecycle, cooling-off windows,
workflow DAG execution, credentials, AgentCard, conformance reports,
SLA divergence, principal binding (bilateral signatures, expiry,
forged unbind), key-rotation chains, full economy flows, and
exhaustive HTTP route coverage (`tests/http_routes.rs`).

## Benchmarks

Key figures (16-core EPYC, release build, SQLite `:memory:`) — full
report and methodology in [`BENCHMARK.md`](./BENCHMARK.md):

| Metric | Value |
|---|---|
| Propose (full path), c=1 | 10,972 req/s (p50 0.08 ms) |
| Propose, c=16 | 14,407 req/s (p50 0.78 ms) |
| `/health`, `/v1/identity`, c=16 | 17,402 – 18,724 req/s |
| Ed25519 sign / verify | 14.0 µs / 40.5 µs |
| SQLite spine append | 4.36 µs (229k ops/s) |

Propose throughput went from **602 → 10,972 req/s at c=1** and from
**41 → 14,407 req/s at c=16** since the first campaign (quadratic-path
fixes + worker pool). Throughput is stable under load; the remaining
ceiling is the global state Mutex ("one process, one order" design).

To reproduce: `cargo bench --bench protocol` (protocol layer),
`cargo run --release --example http_bench 5` (HTTP layer).

## License

Dual-licensed under [MIT](./LICENSE-MIT) or
[Apache-2.0](./LICENSE-APACHE), at your option.

---

*GAP is the gap we close.*
