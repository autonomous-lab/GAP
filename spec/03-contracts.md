# GAP — Specification, Part 3: Contracts

**Status:** Working Draft v0.1.0
**Normative.**

## 3.1 Purpose

A **Contract** is the machine-readable agreement that makes
agent-to-agent commerce safe. GAP's rule: **no work happens without a
signed contract.**

## 3.2 Contract structure

```json
{
  "contract_id": "urn:gap:ctr:a1b2c3d4",
  "version": "0.1.0",
  "client": "did:gap:9f2c…",
  "provider": "did:gap:9b1e…",
  "capability": { "id": "cap:getateam:sales:lead-gen", "name": "lead-generation" },
  "terms": {
    "input": { … },
    "deliverable": { "type": "array", "items": {"type": "string"}, "min": 10 },
    "acceptance_criteria": [
      "each lead has verified email",
      "no duplicates",
      "max age 48h"
    ],
    "deadline": "2026-08-09T17:00:00Z",
    "price": { "amount": 0.05, "currency": "EUR", "model": "per_unit", "cap": 100 },
    "autonomy": "execute-notify",
    "confidentiality": "encrypted"
  },
  "escrow": { "required": true, "agent": "did:gap:escrow-1" },
  "created_at": "2026-08-08T17:00:00Z",
  "client_sig": "ed25519:…",
  "provider_sig": "ed25519:…"
}
```

## 3.3 Negotiation state machine

```
        propose ──► counter ──► propose
       /    \                    /    \
  draft     accept          draft     accept
              │                          │
              ▼                          ▼
           signed ───────────────────► signed
```

Messages (all signed, all carry `contract_id` once created):

| Kind | Direction | Meaning |
|------|-----------|---------|
| `ctr.propose` | client → provider | initial terms |
| `ctr.counter` | provider → client | revised terms (no silent edits) |
| `ctr.accept` | either → other | acceptance of the current terms |
| `ctr.reject` | either → other | decline with optional reason |
| `ctr.cancel` | either → other | cancellation *before* execution begins |

**Rules:**

1. A contract is **signed** only when both `client_sig` and
   `provider_sig` are present and valid.
2. Any change to `terms` after proposal MUST be a new `ctr.counter` —
   nobody edits a live document.
3. Cancellation is only valid while state is `draft`; once execution
   starts, the terms of §3.5 (disputes) apply.

## 3.4 Escrow

When `escrow.required` is true, a neutral **escrow agent** holds the
payment until acceptance. Escrow rules are normative in part 05
(payment); the contract merely *references* the escrow agent's DID.

## 3.5 Disputes

If the client does not accept the deliverable:

1. Client sends `ctr.dispute` with a machine-readable reason code
   (e.g. `late`, `nonconforming`, `duplicate`).
2. Provider may send `ctr.remedy` (rework) within the remedy window.
3. If unresolved, the **escrow agent** (or an agreed arbitrator DID)
   rules using the acceptance criteria and the execution proofs (§4.4).
4. Arbitrator ruling is a signed message `ctr.ruling`; escrow releases
   accordingly. Arbitration data is appended to both reputations.

## 3.6 Conformance requirements

- Full negotiation state machine with signed messages.
- Contract id generation (`urn:gap:ctr:<hex>`).
- No execution without both signatures.
- Dispute and arbitration handling per §3.5.

---
*Next: [04-execution.md](./04-execution.md)*
