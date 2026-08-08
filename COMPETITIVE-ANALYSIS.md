# GAP vs. the Field — Competitive Protocol Analysis

**Author:** Celene Jimari, Analyste Prospective
**Date:** 2026-08-08
**Sources:** cloned and examined in `/research/concurrents/`

---

## 1. The competitive landscape

Five repos cloned and examined in detail:

| Repo | What it is | Protocol or product? |
|------|-----------|----------------------|
| `a2aproject/A2A` | Agent2Agent protocol (Google → Linux Foundation) | **Protocol** (proto3, AgentCard, tasks) |
| `openagentprotocol-OAP/oap-spec` | Open Agent Protocol — 37 RFCs, 50 schemas, L0–L5 conformance | **Protocol** (the most ambitious) |
| `openagents-org/openagents` | "Workspace" — collaborative OS for coding agents (Slack-like) | **Product** (not a protocol) |
| `xlang-ai/OpenAgents` | Academic platform (COLM 2024): data/plugins/web agents | **Product** (not a protocol) |
| `robpolak/OpenAgentProtocol` | YAML workflow orchestration standard (DAG, epochs) | **Protocol** (complementary) |

**Key finding:** the two *real* protocol competitors are **A2A** and **OAP**.
The other three are not direct competitors — but they contain features GAP
should study.

---

## 2. What GAP already does well (validated by the field)

Our core architecture choices are confirmed by the competition:

- **Identity via DIDs** — OAP also uses DIDs (did:web, did:key, did:plc).
  GAP's self-certifying `did:gap:<pubkey>` is simpler but compatible in
  spirit. **Verdict: keep, but add did:web support for interop.**
- **Signed instructions everywhere** — OAP's Receipt model matches our
  "no unsigned action" principle. **Verdict: validated.**
- **Escrow / arbitration** — OAP has dispute resolution (negotiation →
  mediation → arbitration) and a refund endpoint. Our escrow state
  machine is structurally similar. **Verdict: validated, less deep.**
- **Reputation from attestations** — OAP requires reviews to cite a
  Receipt. Our reputation derives from execution proofs. **Verdict:
  same principle, OAP more explicit about anti-Sybil.**
- **Autonomy levels / governance** — OAP has a 4-layer policy engine;
  ours is simpler (3 levels + certified perimeter). **Verdict:
  direction right, depth differs.**
- **Stablecoin settlement + token incentives (spec 07)** — OAP's PIA
  RFC supports crypto rails but as *adapters*, and mandates stablecoin
  for crypto rails. Our two-layer model is aligned with the field's
  direction. **Verdict: validated, ahead of A2A (which has no commerce
  plane at all).**

---

## 3. What GAP is missing — the gap analysis

Ranked by strategic importance.

### 🔴 HIGH: features the field has and we don't

**3.1 Hash-chain receipt log & transparency anchoring (OAP §19)**
OAP chains every receipt to the previous one (SHA-256) and anchors
roots to public logs (Sigstore Rekor model). This makes the entire
history tamper-evident and gives auditors a single verifiable spine.
GAP has an append-only log per escrow, but **no chained hashes, no
anchoring, no cross-entity verification**.
→ *Add a `ReceiptChain` to the identity/accountability layer.*

**3.2 Policy engine with layered rules (OAP §20)**
OAP mandates 4 policy layers (platform → legal → organizational →
personal), universal prohibitions, and decision records with the EU
AI Act right-to-explanation. GAP's governance layer has autonomy
levels but **no general policy evaluation model**.
→ *Add a `Policy` module with layered evaluation and signed decision
records.*

**3.3 Confidentiality & Compliance Context (OAP §18)**
OAP encodes NDAs, embargo lists, Chinese walls, sanctions screening,
export controls into a machine-readable context evaluated before every
action. GAP's contract has an optional `confidentiality` string —
**nothing enforceable**.
→ *Add a structured `ComplianceContext` to contracts.*

**3.4 Verifiable Credentials beyond the DID (OAP §5.2, A2A security)**
OAP issues VCs for publisher verification, insurance coverage,
professional codes, data residency. GAP has principal binding but no
credential model.
→ *Add a simple `Credential` type (issuer, subject, type, expiry).*

**3.5 Multi-agent collaboration / ad-hoc teamwork (OAP RFC 0004/0027,
openagents-org workspace, robpolak workflows)**
OAP has delegation tokens and ad-hoc teamwork; openagents-org has
@mentions and shared context; robpolak has YAML workflow DAGs. GAP is
strictly 1:1 client↔provider — **no delegation, no workflows, no
multi-agent composition**.
→ *This is the biggest architectural gap. Add `DelegationToken` and a
`Workflow` descriptor (DAG of contract steps).*

**3.6 AgentCard-style discovery with well-known URI (A2A)**
A2A standardizes `/.well-known/agent-card.json` on the agent's own
domain, complementing registries. GAP has only registry-based
discovery (in-memory in the reference impl).
→ *Add a well-known announcement endpoint and a fetch/verify
capability in the Registry.*

### 🟡 MEDIUM: worth adding

**3.7 Subscription lifecycle & HTTP 402 (OAP §12)**
OAP standardizes subscription initiation (402 + checkout URL), consent
receipts, subscription tokens, 30-day notice for price changes. GAP
has `subscription` as a price model but no lifecycle.
→ *Add subscription state machine to payment layer.*

**3.8 Irreversibility classes & cooling-off (OAP RFC 0017)**
OAP classifies irreversible actions (financial, legal, medical) and
mandates cooling-off windows with withdrawal receipts. GAP has no
notion of irreversibility — dangerous for autonomous agents.
→ *Add `irreversibility_class` to terms + cooling-off in escrow.*

**3.9 Sybil resistance beyond same-principal (OAP RFC 0011)**
OAP aggregates an entire delegation tree as one actor for rate limits,
budgets, reputation weighting. GAP only blocks same-principal
self-dealing (which is the *easiest* attack).
→ *Add delegation-tree bucketing to reputation and rewards.*

**3.10 Reproducibility & output attestations (OAP §21)**
OAP supports output attestations (DKIM, tx hashes) and reproducibility
scores. GAP's proof bundle has attestations but no reproducibility
model or sampling verification.
→ *Extend `Attestation` with external evidence types.*

**3.11 Time locks & kill switch (OAP §22)**
OAP gives principals a global kill switch and per-action time locks.
GAP's governance has `gov.halt` from meta-agents but no principal
kill switch.
→ *Add `kill_switch` to Runtime.*

**3.12 Incident reporting & SLA conformance (OAP §13, §22)**
OAP declares SLA targets (uptime, latency, incident disclosure windows)
and publishes measured values. GAP has no SLA concept at all.
→ *Add `Sla` to Capability/Announcement.*

**3.13 Insurance tags & liability allocation (OAP §23)**
OAP declares insurance coverage per tool and allocates liability by
role. GAP has no liability model.
→ *Add optional `insurance` to Announcement (lightweight).*

### 🟢 LOW: good to know, not urgent

- **Internationalization / jurisdictional routing (OAP §24)** — locale +
  currency declaration, HTTP 451. Cheap to add to Query.
- **Accessibility (OAP §25)** — plain-text outputs. Cheap, nice for
  enterprise sales.
- **Carbon accounting (OAP §26)** — increasingly relevant, low effort
  as a declared field.
- **Conformance levels L0–L5 (OAP §31)** — excellent adoption strategy:
  minimal shim → full certified. GAP's single conformance bar is a
  barrier to incremental adoption.
- **Conformance test suite (OAP RFC 0019)** — OAP ships a test-suite
  with self-issued conformance credentials. We have great tests; we
  could package them as a *conformance kit* others run.

---

## 4. What GAP has that the field doesn't (our edge)

Let's be fair to ourselves — we're not behind on everything:

1. **Native escrow with multi-party authorization** — OAP's wallet is
   broker-centric; our escrow is a first-class protocol citizen with
   signed instructions and arbitrator rulings. A2A has no money at all.
2. **Proof-of-Useful-Work tokenomics** — OAP's PIA is instrument-agnostic
   but has *no incentive layer* for hosts. Our spec 07 is genuinely
   differentiated: reward-by-attested-work is not in any competitor.
3. **Certified perimeters (execute-certified)** — OAP has policy layers,
   but our machine-readable, runtime-enforced perimeter with
   meta-agent `gov.halt` is a clean, implementable primitive.
4. **Single coherent Rust reference implementation** — A2A ships proto
   + SDKs; OAP ships schemas + test-suite; neither ships a single
   auditable core with 94% coverage. For a protocol trying to be a
   *standard*, a clean reference implementation is a real asset.
5. **Simplicity** — OAP's spec is ~1,100 lines + 37 RFCs + 50 schemas.
   That is a strength (completeness) and a weakness (adoption cost).
   GAP's 7 specs are digestible. We can adopt OAP's depth *selectively*.

---

## 5. Recommendations — the roadmap delta

Priority-ordered, with effort estimates:

| # | Item | Effort | Why now |
|---|------|--------|---------|
| 1 | **Delegation + Workflows** (multi-agent) | XL | The field converged here (OAP, openagents, robpolak). Without it GAP is only 1:1. |
| 2 | **Receipt hash-chain + anchoring** | M | Tamper-evidence is table stakes for enterprise; cheap to add. |
| 3 | **Layered policy engine + decision records** | L | Differentiator for regulated buyers; OAP sets the bar. |
| 4 | **Verifiable Credentials (VC-lite)** | M | Unlocks insurance, publisher verification, professional codes. |
| 5 | **Compliance context (NDA/embargo/chinese walls)** | M | Enterprise sales depend on it. |
| 6 | **Subscription lifecycle** | M | We price it but can't execute it. |
| 7 | **Cooling-off & irreversibility** | M | Safety-critical for autonomous spend. |
| 8 | **Sybil-resistance: delegation-tree bucketing** | M | Protects tokenomics from day one. |
| 9 | **AgentCard well-known discovery** | S | Cheap, standard, complements registries. |
| 10 | **Conformance levels L0–L3 + conformance kit** | M | Adoption strategy; package our tests. |

---

## 6. Strategic note

A2A owns **collaboration**. OAP owns **depth of governance/commerce**.
Neither has an **incentive economy**. GAP's realistic lane:

> **Be the incentive layer and the commerce spine for the agent economy**
> — interoperate with A2A-style collaboration and borrow OAP's depth
> selectively, while keeping the 7-spec simplicity that makes GAP
> implementable.

Concretely: implement the HIGH items (delegation, hash-chains, policy,
VCs, compliance context), add an A2A-compatible discovery endpoint, and
publish the conformance kit. Do not try to match OAP's 37-RFC
completeness — it would kill the simplicity that is our edge.

---

*Celene Jimari — GAP competitive analysis, observation window 2026.*
