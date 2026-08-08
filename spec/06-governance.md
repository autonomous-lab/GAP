# GAP — Specification, Part 6: Governance

**Status:** Working Draft v0.1.0
**Normative.**

## 6.1 Purpose

Governance is the layer that makes GAP **adoptable**. Autonomy without
guardrails is a liability; GAP standardizes how autonomy is granted,
scoped, and supervised — so that enterprises can safely raise an
agent's autonomy level as trust accumulates.

## 6.2 Autonomy levels

| Level | Name | Provider may… | Guardrail |
|-------|------|---------------|-----------|
| 0 | `propose` | prepare work and propose; never commit | human approval required for every action |
| 1 | `execute-notify` | execute; notify humans in parallel | spend & external commitments still need approval |
| 2 | `execute-certified` | execute within a **certified perimeter** | perimeter enforced by the runtime; breach → halt |

The level is **negotiated per contract** (part 03) and **enforced by the
runtime** (part 04) — never self-declared at execution time.

## 6.3 Certification

A **perimeter** is a machine-readable policy:

```json
{
  "cert_id": "urn:gap:cert:b4c2…",
  "agent_did": "did:gap:9f2c…",
  "granted_by": "did:gap:principal-geta",
  "scope": {
    "allowed_actions": ["read.inbox", "write.draft", "send.within.budget"],
    "denied_actions": ["send.external", "spend.over.100"],
    "budget": { "per_day": 100, "currency": "EUR" },
    "channels": ["slack:getateam", "email:inbound"],
    "jurisdictions": ["EU"]
  },
  "valid_from": "2026-08-08T00:00:00Z",
  "valid_until": "2026-09-08T00:00:00Z",
  "grantor_sig": "ed25519:…"
}
```

Message kinds: `gov.certify` (grant), `gov.revoke` (revoke), `gov.renew`.

## 6.4 Meta-agents (supervision)

Supervision is performed by **meta-agents** — agents whose capability is
*governing other agents*. A meta-agent:

1. Watches execution streams (§4.2) for policy violations.
2. Evaluates drift signals: autonomy creep, budget anomalies, anomalous
   counterparties.
3. Emits `gov.alert` (advisory) or `gov.halt` (mandatory stop) —
   `gov.halt` MUST be honored by every compliant runtime.
4. Produces signed supervision reports appended to the supervised
   agent's reputation log.

A meta-agent is itself supervised — chains of supervision are bounded
(at most 3 hops in v0.1) to avoid infinite regress.

## 6.5 Principal rights

The principal (human or organization) always retains, at minimum:

- **Veto:** override any agent decision, even post-hoc.
- **Budget authority:** hard cap on spend, enforced by the runtime.
- **Audit access:** full read access to message log, contracts,
  receipts, and supervision reports.
- **Unbind:** terminate the agent's principal binding (§1.3).

These rights are inalienable — no contract or certificate may waive them.

## 6.6 Conformance requirements

- Enforce autonomy levels at runtime (halt on breach).
- Accept/validate/honor `gov.certify`, `gov.revoke`, `gov.halt`.
- Honor principal veto and budget caps.
- Bounded supervision chains (≤ 3 hops).

---
*End of GAP v0.1.0 specification. Reference implementation: `/src` (Rust).*
