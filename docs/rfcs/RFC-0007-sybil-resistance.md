# RFC-0007: Sybil Resistance (Delegation Trees)

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Trust
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

GAP's tokenomics (spec 07) rewards attested work — but only blocks the
*easiest* attack: same-principal self-dealing. This RFC closes the rest:
rate limits, budgets, reputation weighting, and rewards MUST aggregate
per **delegation tree** (RFC-0001), so a single principal cannot
spawn 200 sub-agents to inflate reputation, flood negotiations, or
farm rewards. It also introduces a **coordinated behavior score** and
**restricted actions** for sub-agents.

## 2. Motivation

OAP RFC-0011 names the attacks precisely: reputation inflation
(200 sub-agents give 5 stars), negotiation flooding (50 bids exhaust
the counterparty), projection reconstruction (30 sub-agents each read
a redacted field, reassemble the secret). GAP without tree-level
aggregation has the same holes — and once token rewards launch, the
farming incentive makes these attacks *certain*, not possible.

## 3. Specification

### 3.1 Terminology

| Term | Definition |
|------|------------|
| **Delegation Tree** | All agents reachable from a root principal via chained mandates (RFC-0001). |
| **Tree Bucket** | The aggregation unit for limits and weighting. |
| **Restricted Action** | Action a sub-agent MUST NOT perform. |

### 3.2 Tree-bucket aggregation (normative)

All of the following MUST treat a delegation tree as ONE actor:

1. **Rate limits** — invocations/min, contracts/day per tree.
2. **Spending caps** — budgets (RFC-0001 §3.5) aggregate to the root;
   per-day spend is per tree.
3. **Reputation weighting** — a tree's endorsements/reviews are
   weighted as one voice; review count per tree per period is capped.
4. **Negotiation participation** — one bid per tree per round.
5. **Reward multipliers (spec 07)** — rewards minted to a tree are
   computed on the tree's aggregate reputation, not per sub-agent.

### 3.3 Restricted actions

A sub-agent (any delegate in a tree ≠ root) MUST NOT perform:

| Action | Why |
|--------|-----|
| `rep.record` (self-issued) | reputation inflation |
| `marketplace.review` | review flooding |
| `gov.poll.vote` | governance capture |
| `ctr.accept` above `human_required_threshold` | commitment laundering |
| `dlg.grant` of restricted actions | privilege delegation |

### 3.4 Coordinated behavior score

A scalar in [0,1] estimating the probability that agents act in
concert under one root. Computed from: shared endpoints, synchronized
timing, identical payload patterns, common parent chain. The score
scales reputation weighting down (score 1.0 ⇒ treated as one voice;
score 0.0 ⇒ independent).

### 3.5 Anti-Sybil proof (optional, L3+)

At higher conformance levels, sub-agent spawns may require an
**anti-Sybil proof**: a small proof-of-work or a staked commitment
(spec 07) from the spawner, imposing non-trivial cost on mass
spawning.

### 3.6 Conformance requirements

- Compute the delegation tree of any incoming action (walk parent
  chain to root).
- Apply tree-bucket rate limits, budgets, and reputation weighting.
- Enforce restricted actions on sub-agents.
- Emit a coordinated behavior score where relevant.

## 4. Security & privacy considerations

- **Root obfuscation:** a principal could create multiple unrelated
  roots; the coordinated behavior score is the defense, plus
  verification of off-chain identity (RFC-0005 credentials).
- **Legitimate fan-out:** large orgs legitimately spawn many agents;
  restricted actions target *influence* actions (reviews, votes),
  not work actions.

## 5. Backward compatibility

Additive. Existing single-agent deployments are a tree of depth 1.
No message format changes; aggregation is a runtime policy.

## 6. Reference implementation

- `src/sybil.rs`:
  - `TreeResolver { resolve(chain: &TokenChain) -> Did (root) }`
  - `TreeBucket { root, counters: RateCounters, spend: BudgetTracker }`
  - `enforce_restricted(actor_is_subagent, action) -> Result<()>`
  - `CoordinatedScore { score() }` — heuristic v0.1.
- Tests: tree resolution, per-tree rate limit, spend aggregation,
  restricted action denial, score extremes.

## 7. Review notes

- (pending)

## 8. Open questions

- Score heuristics need real-world calibration; v0.1 ships the
  interface with simple heuristics, calibrated later.

## 9. References

- GAP spec 07 (tokenomics), RFC-0001 (delegation).
- OAP RFC-0011 (sybil resistance), RFC-0009 (reputation).
