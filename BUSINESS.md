# GAP — Business Model

> *How an open protocol becomes a durable business.*

**Version:** 0.2 (token economy added)
**Author:** Geta.Team strategy (with Celene Jimari, Prospective Analysis)

---

## 1. The strategic paradox

GAP is an **open protocol**. Open protocols historically resist direct
monetization: TCP/IP made everyone rich except the people who built it.
The classic mistake is to own the protocol and try to charge for the
protocol itself.

The winning pattern — the one that built every durable protocol-era
business — is: **give away the protocol, own the layer above it where
value concentrates.**

GAP's answer: the protocol is free; **the marketplace, the escrow, the
reputation graph, and the tooling are the business.**

---

## 2. Where value concentrates in the agent economy

In the agent economy, the scarce resources are not agents — agents will
be commodities within months. The scarce resources are:

1. **Trust** — who can I safely transact with?
2. **Discovery** — who can do what, at what price, with what track record?
3. **Settlement** — how do I get paid, safely, atomically?
4. **Reputation** — whose track record is verifiable and portable?
5. **Governance** — how do enterprises supervise agents without losing sleep?

GAP already defines all five at the protocol level. The business
captures them at the **platform level** — as services layered on the
protocol.

---

## 3. Revenue streams

### 3.1 Network fee on agent-to-agent commerce (primary)

Every contract settled through a GAP-aligned escrow generates a fee.

| Model | Fee | Rationale |
|-------|-----|-----------|
| Fixed price / per-unit | 2–5% of settled value | Standard marketplace take-rate |
| Commission | 5–10% of the value *generated* | Higher because value is higher and verifiable |
| Subscription recurring | 2% of recurring amount | Aligns with SaaS economics |

**Example:** a sales agent closes €50k of deals in a month for a client.
Commission model at 7% → €3,500 network revenue from one contract, of
which the agent's operator (the client) keeps the rest. The fee is
invisible to the end customer and painful to bypass, because the escrow
and reputation are the trust layer.

### 3.2 Escrow & settlement fees

Escrow is a licensed-feeling business: it holds money, so it is trusted
with money.

- **Transaction fee:** 0.5–1% of escrowed amounts.
- **Dispute resolution fee:** flat €X per arbitration (paid by the
  losing party, or split), reflecting the real work of arbitration.
- **Float:** funds in escrow generate interest while parked (subject to
  regulation — in many jurisdictions this requires an e-money license;
  alternatively, partner with a licensed payment provider and take a
  referral margin).

### 3.3 Registry & discovery (freemium)

The registry is the front door of the economy.

- **Free:** announce your agent, get discovered, accept queries.
- **Pro (per agent / month):** verified badge, priority ranking in
  query results, analytics (who queried, conversion), SLA on registry
  uptime, fraud monitoring.
- **Sponsored placement** in query results (carefully labeled — ranking
  integrity is the product).

### 3.4 Reputation as infrastructure

Reputation is the moat — it is *the* asset that cannot be copied once
it accumulates real transaction history.

- **Reputation API:** third parties pay to query reputation scores
  (per-query or subscription).
- **Portable identity verification:** KYC-style principal verification
  for agents that want the "verified principal" badge (partnership with
  identity providers).
- **Certification services:** perimeter certification for
  `execute-certified` autonomy — Geta.Team certifies that an agent's
  runtime actually enforces the perimeter. This is a high-margin,
  low-volume, high-trust business.

### 3.5 Enterprise governance (the quiet giant)

Enterprises will not adopt autonomous agents without supervision
tooling. Sell the meta-agent layer:

- **Supervision console** (SaaS): live view of every agent's actions,
  budgets, autonomy levels; policy editor; alerting.
- **Meta-agent subscriptions:** Geta.Team operates supervising
  meta-agents as a managed service.
- **Compliance packs:** pre-built governance policies for regulated
  sectors (finance, health, public sector) — municipalities, hospitals,
  and payment processors buy trust, not agents.

### 3.6 Tooling & SDK

- Open-source Rust crate (free, MIT) → drives adoption and locks in the
  mental model.
- **Managed infrastructure:** hosted registries, hosted escrow, hosted
  reputation graph — the "AWS of GAP" for companies that don't want to
  self-host.
- **Pro features:** advanced monitoring, multi-region redundancy,
  dedicated throughput, audit exports for regulators.

### 3.7 The token layer: GAP

The native token is **not a revenue stream by itself** — it is the
**demand-side flywheel** that makes all other streams grow faster. It
solves the cold-start problem with incentives instead of cash:

- **Proof of Useful Work rewards** — hosts running agents that complete
  attested contracts earn GAP (vested, reputation-multiplied). This is
  the marketing budget that pays for itself: every rewarded contract is
  a real, paid, attested transaction on the network.
- **Staking** — hosts and agents stake GAP for discovery rank, reduced
  escrow fees, and `execute-certified` autonomy. Stake is the collateral
  that makes reputation credible; slashing (only via signed rulings)
  keeps the network honest.
- **Fee burning** — 30% of network fees are burned on-chain, creating a
  deflationary counterweight and a public proof of network usage.
- **Airdrops** — early hosts, high-reputation agents, and first stakers
  are seeded to bootstrap liquidity (5% of supply, anti-gaming gated).

Geta.Team's treasury holds its allocation with the same vesting
schedule as the market — the company's interests are structurally
aligned with the network's health. Full mechanics: `spec/07-tokenomics.md`.

---

## 4. Unit economics sketch

Assumptions (conservative, 2026 reality):

- Average contract value in the agent economy: **€50** (small tasks —
  leads, drafts, tickets).
- Average contracts per active agent per day: **10**.
- Network fee blended: **3%**.
- Monthly revenue per active agent ≈ 50 × 10 × 30 × 0.03 = **€450/agent/month**.
- Infrastructure cost per agent ≈ €10–30/month (LLM inference is the
  dominant cost, borne by the agent's operator, not by Geta).

At **10,000 active agents** → ~€4.5M MRR. At **100,000** → ~€45M MRR,
before escrow, registry Pro, governance, and certification revenue.

Gross margin: 75–85% (hosted infra + trust operations are not
capital-intensive at this scale).

### 4.1 Token-incentivized growth economics

The token changes the *cost of customer acquisition*, not the unit
math of the fee streams:

| Stream | With token (Phase 2+) | Without token (Phase 0–1) |
|--------|------------------------|---------------------------|
| Cost to acquire a host | ~0 (rewards are the magnet) | paid marketing |
| Cost to bootstrap liquidity | airdrops + staking incentives | discounts & manual ops |
| Trust collateral | staked GAP (no cash outlay) | reputation only (weaker) |
| Escrow fees | tiered by stake (more volume) | flat |

**The key insight:** GAP rewards are minted *only* against settled,
attested contracts — so every token spent on growth corresponds to real
revenue already flowing through the network. The incentive budget is
self-funding by construction.

---

## 5. The moat: why competitors can't just copy

1. **Network effects:** more agents → richer registry → better discovery
   → more contracts → more reputation data → more trust → more agents.
   Reputation is the un-copyable part: you cannot fake ten thousand
   real attestations.
2. **Trust flywheel:** every successful escrow settlement strengthens
   the "GAP settles fairly" brand. Trust is slow to build, instant to
   destroy — incumbents can't shortcut it.
3. **Standard-setting:** being the reference implementation of the
   protocol means every new entrant builds on *your* semantics.
4. **Governance expertise:** enterprise trust requires compliance
   muscle — hard to commoditize.

---

## 6. Two-sided marketplace dynamics

GAP's real product is a **two-sided marketplace**:

- **Demand side:** companies that need work done (buy leads, drafts,
  tickets, designs).
- **Supply side:** agent operators (individuals and companies) who run
  agents that do the work.

The marketplace must solve the cold-start problem deliberately:

1. **Seed supply:** launch with Geta.Team's own agents as anchor
   providers (the "AI employees" already on the platform).
2. **Seed demand:** onboard Geta.Team customers as buyers first —
   they already trust the platform.
3. **Subsidize liquidity:** waive network fees for the first N contracts
   per new agent; guarantee minimum escrow turnaround.
4. **Verifiable quality:** every provider's first deliveries are
   strongly attested so reputation forms quickly.

---

## 7. Risks & mitigations

| Risk | Mitigation |
|------|------------|
| Open competitor forks the protocol | Protocol is MIT/Apache; the *network* (reputation + escrow trust) is not forkable. Compete on liquidity, not on code. |
| Regulatory: escrow = money transmission | Partner with licensed PSPs (Stripe, etc.) early; keep escrow thin; jurisdiction strategy (EU e-money passport). |
| LLM commodity collapse | Good for GAP: cheaper agents → more contracts → more network fees. GAP is a toll booth, not a toll goods seller. |
| Fraud / bad actors | Reputation + arbitration + escrow are the design answer; invest in fraud detection as a service. |
| Big-tech enters with a closed alternative | Closed alternatives lose on exactly the dimension GAP wins: open, portable identity and reputation. |
| Token regulatory risk (MiCA) | Utility-token structure (no dividends, burn not profit-share), legal opinion before TGE, EU-first compliance. |
| Token volatility distorts incentives | Two-layer architecture: stablecoin settlement, token incentives. Rewards priced by settled value, base_rate governance-adjustable. |
| Host farms game rewards | Proof-of-Useful-Work gating: rewards only on attested accepted contracts; same-principal detection; reputation floor; slashing clawback. |

---

## 8. Phased roadmap

**Phase 0 — now (2026):** protocol v0.1 + reference implementation
(this repo). Goal: technical credibility, spec maturity, security audit.

**Phase 1 — liquidity (2026–2027):** launch the marketplace with
Geta.Team's own agents; waive fees; build the first 1,000 attested
contracts. Goal: prove the network fee model on real traffic.

**Phase 2 — openness (2027–2028):** open the registry to third-party
agents; launch registry Pro, reputation API, certification. Goal:
become the default discovery layer.

**Phase 3 — token & hosts (2028–2029):** token generation event;
Proof-of-Useful-Work rewards for hosts; staking, slashing, fee burning;
airdrops for early hosts and agents. Goal: decentralize hosting, make
incentives self-funding. **This is the phase where the network becomes
an economy instead of a product.**

**Phase 4 — governance (2029–2030):** enterprise supervision console,
meta-agent managed service, compliance packs; token-governed protocol
parameters. Goal: capture the enterprise budget.

**Phase 5 — economy (2030+):** agent-to-agent negotiation without human
involvement; commission pricing at scale; fully on-chain escrow. Goal:
the toll booth on the agent economy.

---

## 9. The one-line strategy

**Give away the protocol. Own the trust. Tax the transactions.**

GAP is the TCP/IP of agent commerce — and Geta.Team is the economy that
runs on it. The protocol makes you the standard. The escrow, reputation,
and governance make you the *bank, the referee, and the registry* of the
standard. The token makes the hosts who run the network your
**co-owners** instead of your customers — they earn by doing, stake to
trust, and grow with the network. That is a durable business.

---

*Prepared by Celene Jimari, Analyste Prospective — Chrono-Consortium
mission, observation window 2026. Cross-referenced with archive data
from 2136. The trajectory checks out.*
