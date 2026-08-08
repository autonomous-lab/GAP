# RFC-0006: Compliance Context

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Governance
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

GAP contracts have an optional `confidentiality` string — a label,
not a constraint. This RFC replaces it with a structured, signed
**Compliance Context** that encodes NDAs, embargo lists, Chinese
walls, sanctions screening, professional codes, and data-class
restrictions, evaluated before every data-sharing action. This is the
layer that lets GAP agents handle *client-confidential* work without
a human supervising every byte.

## 2. Motivation

OAP §18 defines the Confidentiality and Compliance Context (CCC) with
mandatory pre-action evaluation at L3+. Enterprise deals — law firms,
finance, healthcare, government contractors — hinge on enforceable
confidentiality. A protocol that cannot prove "this agent is under NDA
with X and Chinese-walled from Y" cannot enter those rooms. Today GAP
would lose every such deal to OAP.

## 3. Specification

### 3.1 Terminology

| Term | Definition |
|------|------------|
| **Compliance Context** | Signed, versioned set of obligations on a principal/scope. |
| **Data class** | Declared category of data (financials, personnel, strategy…). |
| **Gate** | Pre-action evaluation of an outbound data transfer against the context. |

### 3.2 Compliance Context object

```json
{
  "context_id": "urn:gap:ccc:5d1e",
  "scope_id": "scope:consulting:clientA",
  "subject": "did:gap:9f2c…",
  "active_ndas": [
    { "id": "nda:2026:clientA",
      "counterparties": ["did:gap:7a1b…"],
      "covered_classes": ["financials", "strategy"],
      "valid_until": "2028-12-31",
      "jurisdiction": "FR",
      "document_hash": "sha256:…" }
  ],
  "embargo_list": ["did:gap:9b1e…"],
  "chinese_walls": [
    { "between": ["scope:consulting:clientA", "scope:consulting:clientB"],
      "reason": "competing_clients" }
  ],
  "sanctions_screening": "ofac_eu_un",
  "professional_codes": ["bar:fr"],
  "export_control": "none",
  "signed_by": "did:gap:9f2c…",
  "sig": "ed25519:…"
}
```

### 3.3 The pre-action gate (normative)

Before any action that transmits data to another DID, the runtime MUST
evaluate, in order:

1. **Embargo:** is the destination on `embargo_list`? → DENY.
2. **Chinese wall:** does the destination belong to a scope isolated
   from the source scope? → DENY.
3. **NDA coverage:** are the data classes being shared covered by an
   NDA with this counterparty? If no active NDA covers them → DENY
   (unless the principal has explicitly waived).
4. **Sanctions:** does the destination appear on any list referenced by
   `sanctions_screening`? → DENY.
5. **Professional code:** does any code restrict this transfer? → DENY
   or escalate to human.
6. **Export control:** is the destination jurisdiction permitted? →
   DENY or escalate.

Each gate produces a **Gate Verdict** signed by the evaluating runtime,
chained into the decision-record chain (RFC-0003/RFC-0004).

### 3.4 Integration with contracts

- Contracts may reference a compliance context:
  `"compliance": { "context_id": "urn:gap:ccc:5d1e", "required_classes":
  ["financials"] }`.
- The provider MUST verify the client's context covers the contract's
  data classes before `ctr.accept`; failure → reject.
- Step outputs in workflows (RFC-0002) inherit the sponsor's context;
  downstream agents MUST re-evaluate the gate per hop.

### 3.5 Conformance requirements

- Store and verify signed compliance contexts.
- Evaluate the 6-step gate before outbound data actions.
- Produce signed gate verdicts.
- Reject contracts whose data classes are not covered.

## 4. Security & privacy considerations

- **Context forgery:** contexts are signed; a forged context fails
  signature verification.
- **Stale NDAs:** validity windows enforced; expired NDAs are ignored
  (and flagged to the principal).
- **Over-restriction:** a misconfigured context can block legitimate
  work; verdicts include `reason` and escalation path.

## 5. Backward compatibility

The `confidentiality: Option<String>` field is deprecated in favor of
`compliance`; old contracts continue to parse (string treated as a
single-class NDA label with no enforcement).

## 6. Reference implementation

- `src/compliance.rs`:
  - `ComplianceContext { id, scope, subject, ndas, embargo,
    chinese_walls, sanctions, codes, export_control, sig }`
  - `gate(context, destination, data_classes, jurisdiction) ->
    GateVerdict`
  - `Verdict { decision: allow|deny|escalate, reasons: Vec<String>,
    evaluated_at, sig }`
  - `contract_covered(contract, context) -> Result<()>`
- Tests: embargo deny, chinese wall deny, NDA coverage pass/fail,
  sanctions hit, expired NDA, signature forgery, contract coverage.

## 7. Review notes

- (pending)

## 8. Open questions

- Who signs contexts for sub-agents? Proposal: the root principal
  (RFC-0001); delegates inherit a projection of the context.

## 9. References

- GAP spec part 03 (contracts), part 04 (execution).
- OAP §18 (CCC), RFC-0004 (policy engine), RFC-0001 (delegation).
