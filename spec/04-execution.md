# GAP — Specification, Part 4: Execution

**Status:** Working Draft v0.1.0
**Normative.**

## 4.1 Purpose

Execution is where work actually happens — and where trust is earned.
GAP requires that every execution be **observable and provable**.

## 4.2 Execution state machine

```
  signed ──► started ──► progressed* ──► delivered ──► accepted
     │          │                            │             │
     │          ▼                            ▼             ▼
     └───► cancelled                   rejected ◄── dispute ─┘
                                          │
                                          ▼
                                     remedy / rework
```

Messages:

| Kind | Meaning |
|------|---------|
| `exe.start` | provider begins work; includes `plan` (steps, ETA) |
| `exe.progress` | heartbeat; includes step index and optional partial proof |
| `exe.deliver` | deliverable + proof bundle (§4.4) |
| `exe.accept` | client accepts; triggers settlement |
| `exe.reject` | client rejects; moves to dispute (§3.5) |

## 4.3 Autonomy levels at runtime

The `autonomy` field of the contract governs **who may emit execution
messages**:

| Level | Provider may | Human required |
|-------|-------------|----------------|
| `propose` | prepare & propose deliverable | yes, always |
| `execute-notify` | execute; notify human in parallel | for spend/commitment only |
| `execute-certified` | execute within certified perimeter | only for perimeter breach |

Certification of a perimeter is itself a GAP artifact (`gov.certify`,
part 06) — the agent's principal attests which actions are inside the
certified envelope.

## 4.4 Proof bundle

Every `exe.deliver` MUST carry a **proof bundle**:

```json
{
  "contract_id": "urn:gap:ctr:a1b2…",
  "deliverable_hash": "sha256:9f2c…",
  "steps": [
    { "index": 1, "description": "scrape inbound queue", "proof": "ipfs:Qm…", "ts": "…" }
  ],
  "attestations": [
    { "verifier": "did:gap:auditor-1", "sig": "ed25519:…", "verdict": "conforms" }
  ],
  "provider_sig": "ed25519:…"
}
```

- `deliverable_hash` commits to the exact bytes delivered; the client
  recomputes and compares.
- Steps MAY reference external storage (IPFS, object store) for proofs.
- Third-party attestations (auditors, the escrow agent) strengthen the
  bundle but are not required for a valid delivery.

## 4.5 Verification

The client MUST, on receipt:

1. Recompute `deliverable_hash` and compare.
2. Verify `provider_sig`.
3. Check acceptance criteria from the contract (§3.2).
4. Emit `exe.accept` (→ settlement) or `exe.reject` (→ dispute).

## 4.6 Conformance requirements

- Emit `start/progress/deliver` with valid proof bundles.
- Verify deliverable hashes and signatures.
- Enforce the autonomy level on every transition.
- Append every execution outcome to the reputation log (part 01).

---
*Next: [05-payment.md](./05-payment.md)*
