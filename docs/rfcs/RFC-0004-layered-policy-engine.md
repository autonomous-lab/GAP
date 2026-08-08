# RFC-0004: Layered Policy Engine

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Governance
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

GAP's governance today is autonomy levels plus certified perimeters
(spec part 06). This RFC generalizes that into a **layered policy
engine**: every consequential action is evaluated against four policy
layers (platform → legal → organizational → personal), producing a
signed **Decision Record**. This matches OAP §20 and — critically —
satisfies the EU AI Act's right-to-explanation obligations for
autonomous agents.

## 2. Motivation

Regulated buyers (finance, health, public sector) will not deploy
autonomous agents that cannot explain their decisions. OAP mandates a
4-layer policy model with universal prohibitions and decision records
carrying explanations in the principal's language. GAP's autonomy
levels answer "how much may this agent do?" but not "why was this
specific action allowed?" The policy engine answers the second
question — and it is the difference between selling to a startup and
selling to a ministry.

## 3. Specification

### 3.1 Terminology

| Term | Definition |
|------|------------|
| **Policy** | A set of rules evaluated against an action context. |
| **Layer** | A policy source with fixed precedence. |
| **Decision Record** | Signed artifact of one policy evaluation. |
| **Deny** | Terminal outcome; no further evaluation. |

### 3.2 The four layers

Evaluated in order; first `deny` terminates.

| Layer | Source | Examples |
|-------|--------|----------|
| L1 Platform | GAP community block list, universal prohibitions | no CSAM, no autonomous lethal targeting, no mass biometric surveillance |
| L2 Legal | statutes/regulations by jurisdiction | EU AI Act, GDPR, MiCA, OFAC sanctions |
| L3 Organizational | principal's org rules | spend limits, embargo lists, approval chains |
| L4 Personal | principal's own preferences | "never send X to Y", budget caps |

### 3.3 Policy representation

A policy is a set of rules; a rule is a predicate over an **Action
Context**:

```json
{
  "policy_id": "pol:getateam:l4:spend",
  "layer": "personal",
  "rules": [
    { "rule_id": "spend_cap",
      "effect": "deny",
      "if": { "field": "action.amount", "op": "gt",
              "value": { "ref": "principal.budget.per_day" } } },
    { "rule_id": "embargo",
      "effect": "deny",
      "if": { "field": "action.to", "op": "in",
              "value": { "ref": "principal.embargo_list" } } }
  ]
}
```

The Action Context is a flat, typed JSON object built from the
envelope, contract terms, identity, and compliance context (RFC-0006).

### 3.4 Decision Record

Every evaluation produces a signed record:

```json
{
  "decision_id": "urn:gap:pol:3a7b",
  "evaluated_at": "2026-08-08T17:00:00Z",
  "layers_evaluated": ["L1", "L2", "L3", "L4"],
  "applied_rules": ["l1.universal.no_csam", "l4.principal.spend_cap"],
  "outcome": "allow_with_conditions",
  "conditions": ["require_human_review"],
  "explanation": "Spend 45 EUR within daily cap 100 EUR; high-risk class per EU AI Act requires human review",
  "explanation_for_principal": "Votre agent a autorisé un paiement de 45 EUR, dans la limite quotidienne de 100 EUR. Classe à risque élevé : confirmation humaine requise.",
  "action_hash": "sha256:…",
  "sig": "ed25519:…"
}
```

### 3.5 Integration points (normative)

1. **Contract signing** — L4 personal and L3 org policies MUST be
   evaluated before `ctr.accept`/`ctr.propose`.
2. **Escrow park/release** — spend policies MUST be evaluated before
   `pay.park`; release policies before `pay.release`.
3. **Execution** — certified perimeter checks (part 06) become L3/L4
   rules; `gov.halt` (meta-agent) is an L1/L2 rule with terminal deny.
4. **Decision records chain** — records are appended to the same
   hash-chain as receipts (RFC-0003).

### 3.6 Conformance requirements

- Evaluate all four layers in order; first deny terminates.
- Produce signed Decision Records for every consequential action.
- Honor universal L1 prohibitions (non-overridable).
- Provide `explanation_for_principal` in the declared locale.
- Chain decision records (RFC-0003).

## 4. Security & privacy considerations

- **Rule injection:** policies are data, not code; the `if` language is
  a closed expression grammar (no eval).
- **Overbroad L4:** principals may misconfigure; L1/L2 cannot be
  overridden by lower layers — the non-overridable floor is the
  safety net.
- **Explanation leakage:** explanations may reveal internal rules; the
  `explanation_for_principal` field is the sanitized, human-facing
  version.

## 5. Backward compatibility

The autonomy-level system (part 06) remains; the policy engine is a
superset that *implements* autonomy enforcement. Existing `gov.halt`
messages remain valid as L1-level directives. Decision records are
new, additive artifacts.

## 6. Reference implementation

- `src/policy.rs`:
  - `Policy { id, layer, rules }`, `Rule { id, effect, if }`
  - `ActionContext` — typed JSON map
  - `Engine { policies: Vec<Policy> }`
  - `evaluate(context) -> DecisionRecord`
  - Rule language: `{ field, op: eq|ne|gt|gte|lt|lte|in|not_in|contains,
    value: literal|ref }`
  - `DecisionRecord` signed, chained.
- Tests: layer ordering, deny termination, universal prohibition
  non-override, rule language ops, explanation localization hook,
  record chaining.

## 7. Review notes

- (pending)

## 8. Open questions

- Rule language growth: regex match on fields? Proposal: defer to v0.3;
  start with comparisons and set membership.
- Policy distribution: who updates L1/L2? Proposal: GAP community
  publishes signed policy bundles; runtimes pull and cache with
  signature verification.

## 9. References

- GAP spec part 06 (governance, autonomy).
- OAP §20 (policy engine), RFC-0003 (hash chains), EU AI Act Art. 13/14.
