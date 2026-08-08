# RFC-0012: SLAs & Incident Reporting

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Commercial
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

GAP capabilities declare price but no **service level commitments**.
This RFC adds SLAs to capabilities (uptime, latency, incident
disclosure windows) and a signed **Incident Report** flow, so buyers
can compare providers on measured — not claimed — reliability, and
the registry can downgrade persistent underperformers.

## 2. Motivation

OAP §13 mandates SLA declarations with community-measured values and
downgrade on persistent divergence; §22 mandates incident reporting.
Enterprises cannot commit to an agent that offers no uptime target.
GAP currently cannot express "this provider guarantees 99.9% uptime
and discloses incidents within 72h" — a table-stakes feature for
procurement.

## 3. Specification

### 3.1 SLA declaration

Added to `Capability` (and AgentCard, RFC-0010):

```json
{
  "sla": {
    "uptime_target": 0.999,
    "latency_p95_ms": 300,
    "max_call_duration_ms": 30000,
    "supports_streaming": true,
    "supports_async": true,
    "regions": ["eu-west", "eu-central"],
    "incident_disclosure_within_hours": 72,
    "scheduled_maintenance_notice_hours": 168
  }
}
```

### 3.2 Rules (normative)

1. **Declared ≠ measured:** SLA targets are *claims*. Measured values
   are published by probing services or attested by counterparties.
2. **Measurement:** the registry (part 02) MAY run probes; any
   participant MAY submit attested measurements (signed, chained to
   RFC-0003).
3. **Downgrade:** persistent material divergence (e.g. measured uptime
   < declared - 0.005 over 30 days) triggers a registry downgrade of
   the provider's conformance level (RFC-0011) until corrected.
4. **Disclosure:** incidents MUST be published within the declared
   window; non-disclosure is a conformance violation.

### 3.3 Incident report

```json
{
  "incident_id": "urn:gap:inc:4e2f",
  "provider": "did:gap:9b1e…",
  "capabilities": ["cap:data:api"],
  "severity": "major",
  "scope": "Elevated latency in eu-west for 40 minutes",
  "root_cause": "DNS provider failover misconfiguration",
  "mitigation": "Traffic rebalanced; failover script corrected",
  "affected_principals": 182,
  "reported_at": "2026-08-08T14:30:00Z",
  "provider_sig": "ed25519:…"
}
```

### 3.4 Conformance requirements

- Declare SLAs on priced capabilities.
- Accept and verify incident reports.
- Track declared vs measured; trigger downgrade on persistent
  divergence.
- Expose measurement data in discovery results.

## 4. Security & privacy considerations

- **Measurement forgery:** measurements are signed and chained; a
  provider cannot edit its own record.
- **Probe poisoning:** probing services are themselves reputed agents;
  multiple independent probes reduce single-source bias.

## 5. Backward compatibility

Additive: `sla` is optional on capabilities; incident messages are new
kinds (`sla.incident`).

## 6. Reference implementation

- `src/sla.rs`:
  - `Sla { uptime, latency_p95, max_duration, streaming, async,
    regions, disclosure_hours, maintenance_notice }`
  - `IncidentReport { id, provider, capabilities, severity, scope,
    root_cause, mitigation, affected, reported_at, sig }`
  - `SlaTracker { declared, measured: Vec<Measurement>,
    diverges() -> bool }`
  - Tests: divergence detection, incident verify, downgrade trigger,
    measurement attestation.

## 7. Review notes

- (pending)

## 8. Open questions

- SLA credits (financial remedy for breach)? Proposal: v0.3 —
  credit receipts via escrow; v0.1 is declaration + downgrade.

## 9. References

- GAP spec part 02 (discovery), part 04 (execution).
- OAP §13 (SLA), §22 (incidents), RFC-0003 (chains), RFC-0011
  (conformance).
