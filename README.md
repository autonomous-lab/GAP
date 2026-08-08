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
├── README.md          # You are here
├── BUSINESS.md        # The business model: how GAP becomes durable
├── COMPETITIVE-ANALYSIS.md  # GAP vs A2A, OAP, OpenAgents, xlang, robpolak
├── docs/              # The RFC process (like OAP's)
│   └── rfcs/          # RFC-0001 … RFC-0012 (delegation, workflows, …)
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
│   ├── credential.rs  # Verifiable credentials (RFC-0005) [planned]
│   ├── discovery.rs   # Registry & capability announcements
│   ├── agentcard.rs   # Well-known discovery (RFC-0010) [planned]
│   ├── contract.rs    # Contract lifecycle
│   ├── message.rs     # Wire format & addressing
│   ├── payment.rs     # Escrow & settlement (signed instructions only)
│   ├── policy.rs      # Layered policy engine (RFC-0004)
│   ├── delegation.rs  # Delegation tokens (RFC-0001)
│   ├── receipt_chain.rs  # Hash-chained receipts (RFC-0003)
│   ├── governance.rs  # Autonomy levels, certification, meta-agents
│   ├── runtime.rs     # The ergonomic facade binding all layers
│   └── error.rs       # Error taxonomy
├── tests/             # Integration tests (full economy scenarios)
│   └── economy.rs
└── examples/          # Runnable examples
    └── lead_gen.rs
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

## Status

**v0.1.0 — Experimental.** The spec is a working draft; the Rust crate is
a reference implementation to validate the wire format and lifecycle.
Expect breaking changes until v1.0.

## Testing

```bash
cargo test            # 74 unit tests + 3 integration scenarios
cargo clippy          # zero warnings
cargo run --example lead_gen   # end-to-end demo
```

The test suite covers: signature tampering, replay attacks, forged
announcements, forged contract signatures, escrow authorization
(wrong party / unsigned instructions / price caps), TTL expiry,
reputation filtering, dispute arbitration, delegation chains
(escalation, depth, budgets), policy engine (layers, terminal deny,
localized explanations), receipt hash-chains (tamper detection,
redaction), and full happy-path economy flows.

## License

Apache-2.0 (spec) / MIT (reference implementation). See individual files.

---

*GAP is the gap we close.*
