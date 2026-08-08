# GAP — Specification, Part 5: Payment

**Status:** Working Draft v0.1.0
**Normative.**

## 5.1 Purpose

Payment turns the protocol into an economy. GAP specifies a **settlement
layer** that is atomic, escrowed, and auditable. The transport of money
itself is delegated to payment rails (Stripe, SEPA, stablecoins, CBDC,
quantum-credit — whatever the principals agree), but the *protocol-level
state machine* is normative.

## 5.2 The escrow state machine

```
      signed contract
           │
           ▼
   pay.parked (funds in escrow) ──► pay.released ──► pay.settled
           │                              │
           │                              ▼
           └──► pay.refunded        pay.disputed ──► pay.ruled
```

Messages:

| Kind | Meaning |
|------|---------|
| `pay.park` | client instructs escrow agent to hold funds (amount from contract) |
| `pay.parked` | escrow agent confirms funds held |
| `pay.release` | release instruction (after `exe.accept`) |
| `pay.released` | escrow confirms release to provider |
| `pay.refund` | full refund to client (after `ctr.cancel` or `exe.reject` + ruling) |
| `pay.dispute` | hold funds during arbitration |
| `pay.ruled` | escrow splits funds per arbitrator ruling |

**Authorizations (normative):**

| Transition | Authorized sender | Required artifacts |
|------------|------------------|--------------------|
| `pay.park` | the contract's **client** | signed instruction + registered signed contract |
| `pay.release` | the contract's **client** | signed instruction + valid `exe.accept` for the same contract |
| `pay.refund` | the contract's **client** | signed instruction |
| `pay.dispute` | the contract's **client** | signed instruction |
| `pay.ruled` | the **arbitrator** | signed ruling carrying a `split` (client/provider fractions summing to 1.0) |

An instruction from any other DID MUST be rejected with `Unauthorized`.
A `pay.ruled` split that does not sum to 1.0 MUST be rejected.

## 5.3 Escrow agent duties

The escrow agent (a DID, possibly itself an agent — supervised per part
06) MUST:

1. Only act on signed instructions referencing a signed contract.
2. Verify that `exe.accept` or the arbitrator ruling is present before
   any release.
3. Never release more than the contract's price cap.
4. Emit a signed, human-readable receipt for every transition.

## 5.4 Pricing models

The contract's `price.model` is one of:

| Model | Meaning | Settlement trigger |
|-------|---------|--------------------|
| `fixed` | one price for the deliverable | on `exe.accept` |
| `per-unit` | price × accepted units (capped) | on `exe.accept`, computed |
| `subscription` | recurring billing per period | on period acceptance |
| `commission` | % of value generated (e.g. closed deals) | on attested value event |

`commission` requires an **attested value event** — a signed statement
from the client's system (or an auditor) that the value materialized.
This is the model that aligns incentives hardest, and the one GAP
expects to dominate the agent economy.

## 5.5 Receipts & audit

Every escrow transition produces a receipt:

```json
{
  "receipt_id": "urn:gap:rcpt:7c1d…",
  "contract_id": "urn:gap:ctr:a1b2…",
  "event": "pay.released",
  "amount": { "amount": 5.00, "currency": "EUR" },
  "from": "did:gap:9f2c…",
  "to": "did:gap:9b1e…",
  "at": "2026-08-08T17:05:00Z",
  "escrow_sig": "ed25519:…"
}
```

Receipts are append-only and MUST be reproducible from the message log.
This is the audit trail that makes GAP deployable in regulated
organizations.

## 5.6 Conformance requirements

- Implement the full escrow state machine.
- Require signed instructions referencing signed contracts.
- Enforce price caps.
- Emit signed receipts; keep an append-only log.

---
*Next: [06-governance.md](./06-governance.md)*
