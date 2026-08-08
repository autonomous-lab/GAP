# RFC-0003: Receipt Hash-Chain & Anchoring

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Accountability
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

GAP receipts are currently signed and append-only per escrow, but
nothing chains them together or anchors them to an external log. This
RFC makes the entire GAP history **tamper-evident at rest and
cross-entity verifiable**: every receipt cites the hash of its
predecessor (a hash chain), and chain roots are periodically anchored
to public logs (Sigstore Rekor model). This is what turns "we keep
logs" into "we can prove the logs are unaltered."

## 2. Motivation

OAP §19 mandates chained receipts with transparency-log anchoring and
offers zero-knowledge state proofs for GDPR-compliant redaction. The
enterprise buyers GAP targets (municipalities, hospitals, payment
processors) require auditability they can *verify*, not just trust.
A single-entity append-only log is trust; a hash chain anchored to a
public log is **proof**. Without this, GAP's accountability story is
incomplete and its enterprise deals will stall at security review.

## 3. Specification

### 3.1 Terminology

| Term | Definition |
|------|------------|
| **Receipt** | Signed record of a settlement/execution/consent event (part 05). |
| **Chain anchor** | SHA-256 of the previous receipt in the same chain. |
| **Chain root** | The most recent receipt hash of a chain. |
| **Anchor commitment** | Root hash published to an external transparency log. |

### 3.2 Chained receipt

Every receipt gains two fields:

```json
{
  "receipt_id": "urn:gap:rcpt:7c1d…",
  "contract_id": "urn:gap:ctr:a1b2…",
  "event": "pay.released",
  "amount": { "amount": 5.00, "currency": "EUR" },
  "from": "did:gap:9f2c…",
  "to": "did:gap:9b1e…",
  "at": "2026-08-08T17:05:00Z",
  "chain": {
    "previous_hash": "sha256:9f2c…",
    "index": 42,
    "chain_id": "urn:gap:chain:escrow-9f2c"
  },
  "escrow_sig": "ed25519:…"
}
```

### 3.3 Rules (normative)

1. **Every receipt has an index** and cites `previous_hash` (the SHA-256
   of the canonical serialization of the previous receipt, including
   its signature). The first receipt's `previous_hash` is
   `sha256:0000…` (zero hash).
2. **The signature covers the chain fields** — chaining data is part of
   the signed content; tampering with either the receipt or its chain
   link breaks verification.
3. **One chain per logical ledger**: escrow instances, reputation logs,
   and workflow executions each maintain a chain (`chain_id`).
4. **Anchoring:** at least every 24h, or every 1000 receipts (whichever
   comes first), the chain root is committed to a public transparency
   log. The anchor record is itself a receipt (`event:
   "chain.anchor"`).
5. **Verification:** any verifier can walk the chain from any receipt
   to the root, recompute hashes, and check the root against the last
   public anchor.
6. **GDPR redaction:** when the Right to be Forgotten applies, the
   payload fields of a receipt MAY be replaced by a commitment
   (SHA-256 of the redacted payload) — the chain link is preserved.
   Full redaction of chain fields is forbidden.

### 3.4 Transparency log interface

GAP defines a minimal anchoring interface; any Sigstore-Rekor-compatible
log MAY serve as the anchor:

| Operation | Description |
|-----------|-------------|
| `anchor(chain_id, root_hash, at)` | commit root to log |
| `query(chain_id, index)` | retrieve anchor entry |
| `verify_inclusion(chain_id, root_hash)` | verify anchored |

### 3.5 Conformance requirements

- Chain every receipt (index + previous_hash), signed.
- Maintain per-ledger chains.
- Anchor roots on schedule.
- Verify chains and anchors.
- Support payload redaction with commitment replacement.

## 4. Security & privacy considerations

- **Forking:** an attacker with the signing key can rewrite history —
  the chain detects *post-hoc* alteration, not key compromise. Key
  compromise is handled by identity layer (rotation, RFC-0005).
- **Anchor loss:** if the transparency log is unavailable, anchoring
  MUST be retried and flagged; receipts remain valid but the anchor
  lag is reported in the trust score.
- **Privacy:** redaction-by-commitment preserves chain integrity
  without leaking payloads.

## 5. Backward compatibility

Receipts gain a `chain` field; old receipts verify as index-0 roots.
Escrow implementations MUST start chaining on upgrade; the first
chained receipt cites the previous (unchained) one as its
`previous_hash` where available, else the zero hash.

## 6. Reference implementation

- `src/receipt_chain.rs`:
  - `ChainLedger { chain_id, entries: Vec<Receipt> }`
  - `append(receipt) -> Result<Receipt>` (computes hash of previous)
  - `verify_chain() -> Result<()>`
  - `anchor() -> AnchorRecord`
  - `redact(index, commitment) -> Result<()>`
  - `AuditProof { path, root }` — inclusion proof walk.
- Tests: chain integrity, tamper detection (flip any byte → verify
  fails), anchoring roundtrip, redaction keeps chain valid.

## 7. Review notes

- (pending)

## 8. Open questions

- Should GAP operate its own transparency log or integrate with an
  existing Rekor instance? Proposal: interface-first; reference
  implementation ships an in-memory log with Rekor-compatible API.

## 9. References

- GAP spec part 05 (payment receipts), part 04 (execution proofs).
- OAP §19 (receipts, hash chains, transparency log), Sigstore Rekor.
- RFC-0004 (policy decisions are chained too).
