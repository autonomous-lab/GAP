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
├── docs/              # The RFC process (like OAP's)
│   ├── rfcs/          # RFC-0001 … RFC-0012 (delegation, workflows, …)
│   ├── node-api.md    # The GAP node HTTP API (what agents point at)
│   └── deployment.md  # Storage architecture (SQLite / ClickHouse)
├── spec/              # The protocol specification (normative)
│   ├── 00-overview.md
│   ├── 01-identity.md
│   ├── 02-discovery.md
│   ├── 03-contracts.md
│   ├── 04-execution.md
│   ├── 05-payment.md
│   ├── 06-governance.md
│   └── 07-tokenomics.md
├── src/               # Rust reference implementation
│   ├── lib.rs         # Public API surface
│   ├── identity.rs    # DIDs, keys, reputation
│   ├── credential.rs  # Verifiable credentials (RFC-0005)
│   ├── discovery.rs   # Registry & capability announcements
│   ├── agentcard.rs   # Well-known discovery (RFC-0010)
│   ├── contract.rs    # Contract lifecycle
│   ├── message.rs     # Wire format & addressing
│   ├── payment.rs     # Escrow & settlement (signed instructions only)
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
│   ├── storage/       # Storage abstraction + backends
│   │   ├── mod.rs     # Storage trait + conformance suite
│   │   ├── sqlite.rs  # SQLite backend (dev/tests)
│   │   └── clickhouse.rs  # ClickHouse backend + sequencer (prod)
│   ├── governance.rs  # Autonomy levels, certification, meta-agents
│   ├── runtime.rs     # The ergonomic facade binding all layers
│   └── error.rs       # Error taxonomy
├── tests/             # Integration tests (full economy scenarios)
│   └── economy.rs
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

### Scenario 2: a Geta.Team agent and OpenClaw / Hermes

The external agent uses a **GAP adapter** — a small HTTP client (or
MCP server) that exposes the node's API as tools the agent already
understands. The adapter:

1. Announces the external agent's capabilities to the node.
2. Translates GAP contracts into task requests the agent can execute.
3. Submits proof bundles and receives payment confirmations.

The external agent never learns GAP. It sees "a task with terms and a
payment promise" — the adapter speaks GAP for it. (`adapters/` is the
planned home for reference adapters: MCP, OpenClaw, Hermes.)

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

### Full agent instructions

See [`AGENTS.md`](./AGENTS.md) — the 5-step onboarding, the rules of
protocol engagement, and the endpoint quick-reference.

## Status

**v0.1.0 — Experimental.** The spec is a working draft; the Rust crate is
a reference implementation to validate the wire format and lifecycle.
Expect breaking changes until v1.0.

## Testing

```bash
cargo test            # 124 unit tests + 3 integration scenarios
cargo clippy          # zero warnings
cargo run --example lead_gen   # end-to-end demo
```

The test suite covers: signature tampering, replay attacks, forged
announcements, forged contract signatures, escrow authorization
(wrong party / unsigned instructions / price caps), TTL expiry,
reputation filtering, dispute arbitration, delegation chains
(escalation, depth, budgets), policy engine (layers, terminal deny,
localized explanations), receipt hash-chains (tamper detection,
redaction), compliance gates (embargo, chinese walls, NDA), sybil
resistance (tree aggregation, one-bid-per-tree), subscription
lifecycle (consent, renewal, budget), cooling-off windows, workflow
DAG execution, credentials (projection, revocation), AgentCard
(well-known discovery), conformance reports, SLA divergence, and full
happy-path economy flows.

## License

Apache-2.0 (spec) / MIT (reference implementation). See individual files.

---

*GAP is the gap we close.*
