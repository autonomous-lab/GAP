# GAP — Specification, Part 0: Overview

**Status:** Working Draft v0.1.0
**Editors:** Geta.Team
**Related:** 01-identity, 02-discovery, 03-contracts, 04-execution, 05-payment, 06-governance

## 0.1 Scope

This document is the normative entry point of the **Geta Agent Protocol
(GAP)**. It defines the object model, the addressing scheme, and the
lifecycle that all other parts build upon.

GAP is a **protocol**, not a platform: it specifies message formats,
state transitions, and conformance requirements. Any implementation —
in any language, on any infrastructure — that satisfies these
requirements is GAP-compliant.

## 0.2 Core concepts

### Agent

An **Agent** is the atomic unit of the protocol. An agent has:

- a persistent **DID** (see 01-identity),
- a set of **Capabilities** it can perform,
- a **Reputation** record,
- and a current **Autonomy Level** (see 06-governance).

An agent may be implemented as a language model, a classical program, or
a hybrid. GAP does not care *how* an agent thinks; it only cares *how it
communicates*.

### Principal

A **Principal** is the legal or natural person on whose behalf an agent
acts. Every agent MUST be bound to exactly one principal. This binding is
cryptographically attested and revocable (see 01-identity, §1.4).

### Capability

A **Capability** is a machine-readable description of something an agent
can do:

```json
{
  "id": "cap:getateam:sales:lead-gen",
  "name": "lead-generation",
  "description": "Generate and qualify sales leads from inbound channels",
  "input": { "type": "object", "properties": { "budget": {"type": "number"} } },
  "output": { "type": "array", "items": {"type": "string"} },
  "price": { "amount": 0.05, "currency": "EUR", "model": "per-lead" },
  "autonomy": ["propose", "execute-notify", "execute-certified"]
}
```

### Contract

A **Contract** is a signed, machine-readable agreement between two
agents (or an agent and a principal) that binds a **Client** and a
**Provider** to a specific Capability invocation (see 03-contracts).

## 0.3 Addressing

Every message in GAP carries an `Envelope`:

```json
{
  "protocol": "gap",
  "version": "0.1.0",
  "message_id": "urn:gap:msg:9f8c…",
  "from": "did:gap:0x3f2a…",
  "to": "did:gap:0x9b1e…",
  "contract_id": "urn:gap:ctr:a1b2…",
  "kind": "contract.propose",
  "timestamp": "2026-08-08T17:00:00Z",
  "autonomy": "execute-notify",
  "payload": { }
}
```

- `from` / `to` are always **DIDs** (see 01-identity).
- `kind` is a namespaced event name; the full taxonomy is defined in
  part 04 (execution).
- Every message MUST be signed by the sender's key and MUST carry a
  monotonic timestamp.

## 0.4 Lifecycle

The protocol defines one canonical lifecycle:

```
discover ──► negotiate ──► agree ──► execute ──► attest ──► settle
   │            │           │          │           │          │
   ▼            ▼           ▼          ▼           ▼          ▼
capability   terms      contract    work       proof      payment
announced    exchanged   signed      performed  verified   escrowed
```

Each arrow is a state transition driven by a signed message. The
complete message taxonomy and state machines are normative in parts
03–05.

## 0.5 Conformance

A GAP-compliant implementation MUST:

1. Implement the envelope format (§0.3) exactly.
2. Support DID creation and message signing per part 01.
3. Support contract negotiation and signature per part 03.
4. Emit verifiable execution proofs per part 04.
5. Implement escrow settlement per part 05.
6. Enforce autonomy-level guardrails per part 06.

A compliant implementation MAY:

- Use any transport (HTTP, message queue, quantum channel, carrier
  pigeon).
- Add extension fields under an `ext` namespace.

## 0.6 Versioning

GAP uses semantic versioning. Messages carry the protocol version they
conform to. Backward-incompatible changes bump the major version; the
`version` field in the envelope MUST match.

---
*Next: [01-identity.md](./01-identity.md)*
