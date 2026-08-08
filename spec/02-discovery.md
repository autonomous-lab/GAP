# GAP — Specification, Part 2: Discovery

**Status:** Working Draft v0.1.0
**Normative.**

## 2.1 Purpose

Discovery answers the question: **"which agents can do what, and how do
I reach them?"** It is a registry layer. GAP does not mandate a single
registry — any number of registries may exist, and agents may announce
themselves to several.

## 2.2 Announcement

An agent announces its capabilities with a signed `cap.announce`
message:

```json
{
  "envelope": { … , "kind": "cap.announce" },
  "payload": {
    "agent_did": "did:gap:9f2c…",
    "capabilities": [ { "id": "cap:…", "name": "…", … } ],
    "reachability": [
      { "transport": "https", "endpoint": "https://agent.example/gap" },
      { "transport": "mqtt", "endpoint": "mqtt://broker.example/gap/9f2c" }
    ],
    "autonomy_levels": ["propose", "execute-notify", "execute-certified"],
    "languages": ["fr", "en"],
    "regions": ["EU"],
    "ttl_seconds": 86400
  }
}
```

## 2.3 Query

A client queries a registry:

```json
{
  "envelope": { …, "kind": "cap.query" },
  "payload": {
    "wanted": { "name": "lead-generation" },
    "filters": { "languages": ["fr"], "regions": ["EU"], "max_price": 0.10 },
    "required_autonomy": "execute-notify",
    "min_reputation": 0.9,
    "max_results": 20
  }
}
```

The registry MUST return only agents whose announcements satisfy the
filters, ordered by (relevance, reputation, price) — the exact ranking
is registry policy, but filtering MUST be exact.

## 2.4 Registry responsibilities

A GAP registry MUST:

1. Verify the signature on every `cap.announce` before storing it.
2. Enforce `ttl_seconds` (expire stale announcements).
3. Return signed query results, so clients can verify the registry
   actually held the announcement (prevents registry-side tampering).
4. Support at least one reachability transport listed by the agent.

## 2.5 Deregistration & updates

- **Update:** re-announce with the same `agent_did`; the new
  announcement atomically replaces the old one.
- **Deregister:** send `cap.deregister` signed by the agent.
- **Inactivity:** announcement expires at `ttl_seconds`; the registry
  SHOULD keep a tombstone so queries can distinguish "gone" from
  "never existed".

## 2.6 Conformance requirements

- Publish/refresh capability announcements with TTL.
- Query registries with exact filters.
- Verify registry signatures on query results.
- Handle expiry and deregistration.

---
*Next: [03-contracts.md](./03-contracts.md)*
