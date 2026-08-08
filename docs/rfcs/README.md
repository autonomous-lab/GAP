# GAP RFC Process

> *How GAP evolves — seriously.*

The RFC process is the mechanism by which GAP changes. It mirrors the
discipline of mature protocol bodies (OAP, IETF, W3C): every
significant change is a numbered, reviewed, versioned document.

## Status lifecycle

```
Draft ──► Proposed ──► Accepted ──► Implemented ──► Final
  │           │            │
  └───────────┴────────────┴──► Rejected
```

| Status | Meaning |
|--------|---------|
| `Draft` | Being written; open for comment. |
| `Proposed` | Submitted for review; requires a review window (7 days minimum). |
| `Accepted` | Approved in principle; implementation may begin. |
| `Implemented` | Reference implementation merged; conformance tests pass. |
| `Final` | Normative; breaking changes require a new major RFC. |
| `Rejected` | Withdrawn or declined, with rationale recorded. |

## Rules

1. **Every RFC has a number** — sequential, never reused.
2. **Every RFC has an editor** and (for Accepted+) a **reviewer** from a
   different area of the codebase.
3. **No RFC reaches Final without conformance tests** in the reference
   implementation (Rust) and, where applicable, the test suite.
4. **RFCs are immutable once Final.** Amendments are new RFCs that
   supersede.
5. **The maintainer (Geta.Team) decides, the community reviews.**
   Review comments are recorded in the RFC file under `Review notes`.

## RFC template

See [`TEMPLATE.md`](./TEMPLATE.md). Every RFC MUST fill all sections;
sections marked optional may be omitted only with editor approval.

## Current RFCs

| # | Title | Status | Area |
|---|-------|--------|------|
| 0001 | Delegation Tokens | Draft | Coordination |
| 0002 | Workflow Composition | Draft | Coordination |
| 0003 | Receipt Hash-Chain & Anchoring | Draft | Accountability |
| 0004 | Layered Policy Engine | Draft | Governance |
| 0005 | Verifiable Credentials | Draft | Identity |
| 0006 | Compliance Context | Draft | Governance |
| 0007 | Sybil Resistance (Delegation Trees) | Draft | Trust |
| 0008 | Subscription Lifecycle | Draft | Commercial |
| 0009 | Irreversibility & Cooling-Off | Draft | Safety |
| 0010 | Well-Known Discovery (AgentCard) | Draft | Discovery |
| 0011 | Conformance Levels & Kit | Draft | Process |
| 0012 | SLAs & Incident Reporting | Draft | Commercial |

## How to propose

1. Copy `TEMPLATE.md` to `RFC-00XX-slug.md`.
2. Fill every section.
3. Open a PR with the RFC and a summary of the problem it solves.
4. The review window opens; comments go in the PR and are distilled into
   the RFC's `Review notes`.
5. On acceptance, the reference implementation PR may land alongside.

---
*Process established by Geta.Team, 2026-08-08.*
