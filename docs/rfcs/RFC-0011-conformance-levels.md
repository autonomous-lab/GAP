# RFC-0011: Conformance Levels & Kit

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Process
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

GAP currently has one conformance bar: you either implement the spec
or you don't. This RFC introduces **five conformance levels** (L0–L4)
so that implementers can adopt GAP incrementally — from a minimal
discovery shim to a fully certified regulated deployment — and ships
the **Conformance Kit**: the existing test suite packaged so anyone
can self-certify and publish a signed conformance credential
(RFC-0005).

## 2. Motivation

OAP's tiered conformance (L0–L5) is one of its smartest adoption
moves: a small tool can claim L1 (identity + discovery + invocation)
without implementing the full commercial plane. GAP's all-or-nothing
bar is a barrier: a lead-gen agent that only wants discovery+contracts
must implement escrow+governance first. Tiers create an adoption
ladder, and a conformance kit turns "we wrote tests" into "anyone can
prove conformance" — credibility that matters when claiming to be a
standard.

## 3. Specification

### 3.1 Conformance levels

| Level | Must implement | Suitable for |
|-------|----------------|--------------|
| **L0** | Envelope format (part 00) + DID identity (part 01) | minimal shim, message interoperability |
| **L1** | L0 + discovery announce/query (part 02) + AgentCard (RFC-0010) | directory participants |
| **L2** | L1 + contracts (part 03) + execution + proof bundles (part 04) | working agents, 1:1 services |
| **L3** | L2 + escrow (part 05) + governance/autonomy (part 06) + policy engine (RFC-0004) | commercial participants, enterprises |
| **L4** | L3 + tokenomics settlement (spec 07) + delegation (RFC-0001) + compliance (RFC-0006) + full accountability (RFC-0003) | regulated deployments, network anchors |

### 3.2 Level rules

1. Higher levels are strict supersets — L4 implies L3 implies L2…
2. A participant MUST declare its level in its AgentCard
   (`"conformance": "L3"`) and announcements.
3. Claiming a level without passing its suite is a conformance
   violation with reputation consequences (reported, weighted down).
4. Levels are re-verified periodically; material drift downgrades the
   level (mirrors OAP's downgrade mechanism).

### 3.3 The Conformance Kit

Packaged from the existing test suite:

- `gap-conformance` — a CLI that runs the full suite against a target
  implementation and emits a machine-readable report.
- Reports are signed by the *implementation key* (self-issued, per
  OAP's model) and MAY be co-signed by peer witnesses or anchored in
  the registry (RFC-0003).
- The report includes: level claimed, tests run/passed per area,
  coverage stats, implementation version, timestamp.

### 3.4 Conformance credential

Passing a level produces a `gap.conformance` credential (RFC-0005):

```json
{
  "type": "gap.conformance",
  "claims": { "level": "L3", "suite_version": "0.2.0",
              "tests_passed": 214, "coverage": 0.94 },
  "valid_until": "2027-08-08T00:00:00Z"
}
```

### 3.5 Conformance requirements

- Implement at least the mandatory items of the claimed level.
- Pass the Conformance Kit suite for that level.
- Publish the level in AgentCard/announcements.
- Re-verify on material changes.

## 4. Security & privacy considerations

- **Self-certification gaming:** levels are self-issued; the *market*
  polices via reputation and sampling (OAP-style trust service).
  L4 additionally requires peer-witnessed co-signature.
- **Suite blind spots:** the kit is open source; auditability is the
  mitigation, plus the test suite grows with each RFC.

## 5. Backward compatibility

All existing implementations are L2 (they implement identity,
discovery, contracts, execution). The reference implementation will
target L4.

## 6. Reference implementation

- `src/conformance.rs`:
  - `Level` enum with `required_areas()`
  - `ConformanceReport { level, suite_version, per_area:
    Vec<AreaResult>, passed: bool, sig }`
  - `ConformanceRunner` — aggregates existing test modules per level.
- CLI: `gap-conformance --level L3` → report + credential.

## 7. Review notes

- (pending)

## 8. Open questions

- Who operates the peer-witness network for L4? Proposal: Geta.Team
  anchors initially, opens to the community later.

## 9. References

- GAP spec parts 00–06, spec 07 (tokenomics).
- OAP §31 (conformance levels), RFC 0019 (conformance testing),
  RFC-0005 (credentials), RFC-0003 (anchoring).
