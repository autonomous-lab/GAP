# RFC-0001: Delegation Tokens

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Coordination
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

GAP is currently strictly 1:1: one client, one provider, one contract.
This RFC introduces **Delegation Tokens** — signed, scoped,
transferable authorities that allow an agent to act on behalf of
another agent (or principal) within a bounded mandate. Delegation is
the primitive that unlocks multi-agent coordination, sub-agent
hierarchies, and eventually workflow composition (RFC-0002).

## 2. Motivation

The competitive field converged on delegation as a core primitive:
OAP (RFC-0004) defines delegation tokens with sub-tree aggregation,
openagents-org relies on agent-to-agent task passing, and robpolak
composes multi-agent workflows. GAP without delegation cannot express:

- a **lead agent** that hires a **data agent**, which hires a
  **scraping agent**,
- a **principal** granting an agent the right to sign contracts up to
  a budget without per-contract approval,
- **sub-agents** that participate in negotiations on behalf of a
  parent without full signing power.

Without delegation, every multi-agent interaction degenerates into
manual orchestration — precisely the problem GAP exists to solve.

## 3. Specification

### 3.1 Terminology

| Term | Definition |
|------|------------|
| **Delegator** | The agent/principal granting authority. |
| **Delegate** | The agent receiving authority. |
| **Mandate** | The signed, bounded description of what the delegate may do. |
| **Delegation Tree** | All delegates reachable from a root principal through chained mandates. |
| **Root** | The origin principal of a delegation chain. |

### 3.2 The Delegation Token

A delegation token is a signed document:

```json
{
  "delegation_id": "urn:gap:dlg:a1b2c3",
  "delegator": "did:gap:9f2c…",
  "delegate": "did:gap:9b1e…",
  "root": "did:gap:9f2c…",
  "parent": "urn:gap:dlg:0",
  "mandate": {
    "capabilities": ["cap:getateam:sales:lead-gen", "cap:data:scrape"],
    "budget": { "per_contract": 50, "per_day": 200, "currency": "EUR" },
    "autonomy_level": "execute-notify",
    "jurisdictions": ["EU"],
    "channels": ["email"],
    "expires_at": "2026-09-08T00:00:00Z"
  },
  "revocations": [],
  "issued_at": "2026-08-08T00:00:00Z",
  "delegator_sig": "ed25519:…"
}
```

### 3.3 Rules (normative)

1. **Bounded:** a mandate MUST specify capabilities, budget, autonomy
   level, and expiry. An unbounded mandate is a conformance violation.
2. **Chained:** every token has a `parent`. The root's parent is
   `"urn:gap:dlg:0"` and the root's `root` field is its own DID.
3. **No escalation:** a delegate MUST NOT grant a mandate broader than
   its own. Delegated budget ≤ parent budget; delegated autonomy ≤
   parent autonomy; delegated capabilities ⊆ parent capabilities.
   A delegate that violates this violates conformance and its mandate
   is revoked automatically.
4. **Chain depth:** max 5 hops (matches the delegation rate limit in
   Geta.Team's delegate skill; prevents unbounded recursion).
5. **Signed at every hop:** each token in the chain is signed by its
   delegator. Verification walks the chain to the root.
6. **Revocable:** any delegator in the chain MAY revoke by publishing
   `dlg.revoke`; revocation cascades to all descendants.
7. **Presentable:** a delegate presents its token chain when acting;
   counterparties verify the full chain before accepting a signed
   contract or instruction.

### 3.4 Message kinds

| Kind | Direction | Meaning |
|------|-----------|---------|
| `dlg.grant` | delegator → delegate | issue a mandate |
| `dlg.accept` | delegate → delegator | accept (optional, recommended) |
| `dlg.revoke` | any chain member → registry | revoke a token (cascades) |
| `dlg.present` | delegate → counterparty | present chain for verification |

### 3.5 Budget enforcement

Budget is enforced at the **root level**: all contracts signed by any
delegate in a tree count against the root's budget. Enforcement points:

1. Contract negotiation MUST check the delegate's remaining budget
   (per-contract and per-day, aggregated up the tree).
2. Escrow park MUST re-check the same budget.
3. Violation ⇒ `ctr.reject` with `budget_exceeded`.

### 3.6 Conformance requirements

- Issue bounded, signed mandates.
- Enforce no-escalation on chaining.
- Walk and verify delegation chains (max 5 hops).
- Aggregate budgets up the tree; reject over-budget contracts.
- Support cascade revocation.

## 4. Security & privacy considerations

- **Sybil:** delegation trees are the natural unit for Sybil
  resistance (see RFC-0007): rate limits, reputation weighting, and
  reward multipliers aggregate per tree.
- **Lateral movement:** a compromised delegate can only act within its
  mandate; budgets and capability whitelists contain the blast radius.
- **Replay:** tokens carry `issued_at` and `expires_at`; presenting an
  expired chain is rejected.
- **Privacy:** a delegate need not disclose sibling branches; chain
  verification only needs the ancestor path.

## 5. Backward compatibility

No existing message is modified. New kinds are additive. Contracts
grow an optional `delegation` field referencing the presenting chain.
Existing 1:1 flows are unchanged (root = self).

## 6. Reference implementation

New module `src/delegation.rs`:

- `DelegationToken { id, delegator, delegate, root, parent, mandate,
  sig }`
- `Mandate { capabilities, budget, autonomy, jurisdictions, channels,
  expires }`
- `TokenChain { tokens: Vec<DelegationToken> }` with
  `verify(root: &Did) -> Result<()>`, `is_expired()`, `budget_left()`
- `enforce_no_escalation(parent: &Token, child: &Token) -> Result<()>`
- `BudgetTracker` aggregating spends per tree per day.
- Tests: chain verify, escalation rejection, expiry, cascade revoke,
  budget aggregation.

## 7. Review notes

- (pending)

## 8. Open questions

- Should mandates support `standing` (repeatable) vs `one-shot`?
  Proposal: yes, via `mode: "standing" | "one_shot"` — one-shot
  mandates auto-expire after first use.
- Where is the revocation registry anchored? Proposal: same registry
  as discovery (part 02), with tombstones.

## 9. References

- GAP spec part 00 (§0.2 Principal), part 03 (contracts), part 06
  (autonomy levels).
- OAP RFC-0004 (delegation), RFC-0011 (sybil resistance).
- Geta.Team delegate skill (operational analogue, 5-hop limit).
