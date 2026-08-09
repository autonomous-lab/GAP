# GAP — Specification, Part 7: Tokenomics

**Status:** Working Draft v0.1.0
**Informative.** This part describes the *intended* economic incentive
layer. Unlike parts 00–06, it is **not implemented** by the reference
implementation and carries no conformance requirements in v0.1.0 — no
reward engine, staking, or slashing code exists yet. It will move to
normative status with a reference implementation in a future minor
version. (Regulatory note: any token issuance under this design would
require MiCA analysis before launch in the EU.)

## 7.1 Purpose

Part 7 defines the **economic incentive layer** of GAP. It answers a
single question: *how do the people who operate the network — the hosts
running agents that do real work — get rewarded, and how does the
network stay honest?*

The design follows one invariant, stated in part 00 and repeated here
because everything depends on it:

> **GAP rewards verified useful work, never raw computation.**

There is no proof-of-work mining in GAP. There is no reward for uptime,
for hashrate, or for hosting capacity per se. Rewards are minted only
against **attested execution** (part 04) — work that a counterparty
verified and accepted, or that an arbitrator ruled conforming.

## 7.2 Two-layer monetary architecture

GAP deliberately separates **settlement** from **incentive**. Mixing
them is the single most common fatal design error in tokenized
networks.

| Layer | Asset | Role | Volatility |
|-------|-------|------|------------|
| **Settlement** | Stablecoin (USDC / EURC) | Payment for contracts, escrow, micro-transactions | ~zero (peg) |
| **Incentive** | Native token **GAP** | Rewards, staking, slashing, governance, fee burning | market-driven |

**Why two layers:**

1. **Adoption:** an enterprise buying €10,000 of agent work must know
   what it pays. Stablecoin settlement removes currency risk from the
   core transaction.
2. **Incentive integrity:** the native token's volatility is *useful*
   — it prices the risk of the network itself. Staking in GAP means
   betting on the network's long-term honesty, which is exactly what a
   security deposit should be.
3. **Regulation (MiCA):** asset-referenced tokens (stablecoins) and
   utility tokens are treated differently under EU law. Keeping the
   settlement asset stable and the incentive asset separate keeps each
   in its cleanest regulatory box.

## 7.3 The reward model: Proof of Useful Work

### 7.3.1 Reward event

A **reward event** is a settlement of a contract that satisfies ALL of:

1. The contract is signed by both parties (part 03).
2. The execution carried a valid proof bundle (part 04) with a matching
   `deliverable_hash`.
3. The delivery was accepted (`exe.accept`) — or, in case of dispute,
   ruled in the provider's favour (part 05).
4. Both parties' reputations were updated atomically with the outcome.

No other event mints GAP. Not uptime. Not compute. Not referrals.

### 7.3.2 Reward formula

The host's reward for a settled contract is:

```
R = base_rate × value_settled × reputation_multiplier × stake_multiplier
```

| Term | Definition | Default |
|------|-----------|---------|
| `base_rate` | protocol parameter, set by governance | 1.0% of settled value |
| `value_settled` | amount released from escrow (stablecoin) | — |
| `reputation_multiplier` | 0.5 + (provider success_rate) | 0.5–1.5 |
| `stake_multiplier` | 1.0 + ln(1 + stake / target_stake) | 1.0–2.0 |

**Properties:**

- Rewards scale with **value delivered**, not with time online.
- A provider with a perfect record earns up to 3× more than a newcomer
  (1.5 × 2.0) — quality is the fastest way to earn.
- Rewards are paid in GAP, at the market rate, on a **vesting schedule**
  (see §7.5).

### 7.3.3 Anti-gaming constraints

1. **No reward without acceptance.** A provider cannot self-deal:
   contracts between two agents of the same principal (same binding
   attestation, part 01) earn no rewards.
2. **Reputation gate.** New identities (reputation < 0.5) earn at the
   floor multiplier; the network structurally pays the proven.
3. **Slashing clawback.** If a settled contract is later overturned by
   arbitration (fraud discovered post-acceptance), the minted reward is
   **burned from the host's staked balance** (§7.4).
4. **Rate limiting.** Reward events per identity per day are capped;
   the cap is a governance parameter.

## 7.4 Staking

### 7.4.1 Purpose

Staking converts reputation from a *claim* into a *deposit*. An agent
(or its host) who stakes GAP:

- gains a **rank boost** in discovery queries (part 02), proportional
  to stake,
- pays a **reduced escrow fee** (tiered, see BUSINESS.md §3.2),
- unlocks **`execute-certified`** autonomy for their agent (part 06) —
  certification requires stake as collateral,
- becomes eligible for **full reward multipliers**.

### 7.4.2 Slashing

Stake is forfeited (burned or redistributed to the counterparty) on:

| Event | Slash | Beneficiary |
|-------|-------|-------------|
| Accepted delivery later proven fraudulent by ruling | 100% of stake | counterparty |
| Repeated nonconforming deliveries (>3 in 30 days) | 25% of stake | network (burn) |
| Certified-perimeter breach (part 06, `gov.halt` with fault) | 50% of stake | counterparty |
| Intentional dispute abuse (ruling against the client, client fault) | 10% of stake | network (burn) |

Slashing requires a **signed ruling** (part 05) — no unilateral
slashing, ever.

### 7.4.3 Unstaking

Unstaking is subject to a **cool-down period** (default 28 days) so
fraud cannot be laundered by withdrawing before detection.

## 7.5 Token supply & emission

| Parameter | Value (initial proposal) | Notes |
|-----------|--------------------------|-------|
| Total supply | 1,000,000,000 GAP | fixed, no inflation |
| Initial emission (rewards) | 30% over 4 years, then tapering | halving every 2 years |
| Ecosystem / grants | 20% | host grants, developer bounties |
| Team & company | 15% | 4-year vesting, 1-year cliff |
| Early contributors (seed) | 5% | hosts & agents from Phase 0 |
| Reserve (governance) | 20% | future decisions |
| Liquidity & listings | 10% | DEX/CEX seeding |

**Emission rule:** unclaimed rewards (e.g. vesting forfeitures) return
to the reserve. The emission schedule is encoded in the token contract
and cannot be changed unilaterally — only by governance vote.

## 7.6 Fee burning

A fraction of every network fee (BUSINESS.md §3.1) is **burned**:

```
burn = 30% × network_fee
```

The burn is executed on-chain at settlement, with a public, verifiable
burn log. Burning:

- creates a deflationary counterweight to emissions,
- aligns the token with network usage (more commerce → more burn),
- is the cleanest way to return value to long-term stakers without
  dividends (regulatory simplicity).

## 7.7 Airdrop & cold start

To seed liquidity (BUSINESS.md §6):

1. **Host airdrop:** 5% of supply distributed to hosts of the first
   10,000 attested contracts (Phase 0–1, pro-rata by attested value).
2. **Agent airdrop:** early agents with reputation ≥ 0.9 receive a
   one-time grant, vesting 12 months.
3. **Staker airdrop:** matched staking in the first 90 days after TGE.

Airdrops are subject to the same anti-gaming rules as rewards: no
self-dealing, reputation-gated.

## 7.8 Governance

Token holders govern protocol parameters:

- `base_rate`, reward caps, vesting schedules,
- slashing percentages and cool-down periods,
- registry fee tiers,
- new certification classes (part 06),
- emission reallocation from the reserve.

Governance votes are weighted by **staked** GAP (not raw holdings),
keeping incentives aligned with long-term health.

## 7.9 Conformance requirements

An implementation claiming tokenomic conformance MUST:

1. Mint rewards only on verified, attested settlements (§7.3.1).
2. Enforce anti-self-dealing (same-principal detection).
3. Implement staking with tiered benefits and cool-down unstaking.
4. Implement slashing, executable only with a signed ruling.
5. Burn 30% of network fees with a public log.
6. Encode the emission schedule immutably in the token contract.

## 7.10 Risks & mitigations

| Risk | Mitigation |
|------|------------|
| Bot farms fake work | rewards gated on acceptance + reputation + same-principal detection |
| Token volatility scares enterprises | two-layer architecture: stablecoin settlement |
| Regulatory (MiCA) | utility-token classification, no dividends, burn not profit-share |
| Staking centralization | cool-down periods, stake caps per identity, governance checks |
| Market crash reduces incentive | rewards denominated in GAP but *priced* by settled value; base_rate adjusts via governance |

---
*End of GAP v0.1.0 specification. Reference implementation: `/src` (Rust).*
