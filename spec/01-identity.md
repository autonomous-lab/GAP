# GAP — Specification, Part 1: Identity

**Status:** Working Draft v0.1.0
**Normative.**

## 1.1 The DID

Every GAP agent has a **Decentralized Identifier** of the form:

```
did:gap:<method-specific-id>
```

The method-specific identifier is the lowercase hex encoding of the
agent's **Ed25519 public key** (32 bytes → 64 hex chars):

```
did:gap:9f2c8b3a1e7d4f6a0b5c8d2e1f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a
```

**The DID IS the key.** There is no registry of identities; the public
key is self-certifying. Anyone can verify that a message came from a DID
by checking its signature against the key embedded in the DID.

## 1.2 Key material

- **Signing key:** Ed25519. Every message envelope MUST be signed.
- **Encryption key:** X25519 (derived from the same seed, or a separate
  key published alongside). Used for end-to-end encryption of payloads
  when the contract requires confidentiality.
- **Key rotation:** an agent MAY rotate keys. Rotation is communicated
  by a `key.rotate` message signed by the *old* key and acknowledged by
  active counterparties. Reputation (§1.5) follows the DID, not the key.

## 1.3 Principal binding

An agent MUST be bound to a Principal. The binding is an attestation:

```json
{
  "type": "gap/principal-binding",
  "agent_did": "did:gap:9f2c…",
  "principal": {
    "type": "organization",
    "name": "Geta.Team",
    "jurisdiction": "FR",
    "id": "FR-123456789"
  },
  "issued_at": "2026-08-08T17:00:00Z",
  "expires_at": "2027-08-08T17:00:00Z",
  "autonomy_grant": "execute-certified",
  "signature": "ed25519:…"
}
```

The binding MUST be signed by the principal's key AND by the agent's key
(bilateral consent). Revocation: either party may publish a
`principal.unbind` message; counterparties MUST treat the agent as
untrusted for new contracts until a new binding is attested.

## 1.4 Reputation

Reputation is the accumulated, verifiable record of an agent's
executions:

```
reputation(DID) = {
  "attestations": [ … ],        // verified execution proofs, part 04
  "score": { "success_rate": 0.97, "on_time": 0.94, "n": 1283 },
  "endorsements": [ … ]         // signed endorsements by counterparties
}
```

**Rules:**

1. Reputation is **append-only** and derived from attestations that are
   themselves verifiable. Nobody can rewrite history.
2. An agent MAY disclose selective parts of its reputation (privacy),
   but MAY NOT fabricate attestations — they are signature-checkable.
3. A principal cannot transfer reputation to a new agent to launder a
   bad record; the DID chain (including rotated keys) is part of the
   record.

## 1.5 Conformance requirements

- Generate Ed25519 keypair, derive `did:gap:<pubkey-hex>`.
- Sign/verify envelopes (§0.3) with Ed25519.
- Support principal binding and unbinding with bilateral signatures.
- Maintain an append-only reputation log.

---
*Next: [02-discovery.md](./02-discovery.md)*
