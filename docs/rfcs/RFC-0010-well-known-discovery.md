# RFC-0010: Well-Known Discovery (AgentCard)

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Discovery
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

GAP discovery is registry-based. This RFC adds the complementary
pattern the field standardized: every agent publishes a signed
**AgentCard** at `/.well-known/gap-agent.json` on its own domain, so
any client can discover and verify an agent *without* a registry.
Registries remain for aggregation; well-known discovery is for
direct, domain-rooted trust.

## 2. Motivation

A2A standardized `/.well-known/agent-card.json`; OAP mandates
`/.well-known/oap-tool.json` manifests. Domain-rooted discovery is how
enterprises verify agents they already know ("is this really
weatherpro.example's agent?"). GAP's registry-only model forces
trust in the registry; well-known discovery gives agents a
self-owned address. It also makes GAP interoperable with A2A-style
discovery conventions — a cheap credibility win.

## 3. Specification

### 3.1 The AgentCard

Published at `https://{domain}/.well-known/gap-agent.json`:

```json
{
  "gap_version": "0.2.0",
  "agent": {
    "did": "did:gap:9f2c…",
    "name": "Weather Pro Agent",
    "description_for_agents": "Provides weather forecasts and climate data. Deterministic, no marketing language, ≤ 4000 chars.",
    "provider": { "did": "did:gap:verifier-1", "legal_name": "Weather Pro GmbH" }
  },
  "capabilities": [
    { "id": "cap:weather:forecast", "name": "weather-forecast",
      "price": { "amount": "0.001", "currency": "EUR", "model": "per_call" },
      "irreversibility_class": "reversible" }
  ],
  "endpoints": {
    "invoke": "https://api.weatherpro.example/gap/invoke",
    "discover": "https://api.weatherpro.example/gap/discover",
    "billing": "https://api.weatherpro.example/gap/billing"
  },
  "auth": ["bearer", "oauth2"],
  "autonomy_levels": ["propose", "execute-notify"],
  "jurisdictions": ["FR", "DE"],
  "credentials": ["urn:gap:vc:8f3a"],
  "updated_at": "2026-08-08T00:00:00Z",
  "agent_sig": "ed25519:…"
}
```

### 3.2 Rules (normative)

1. **Self-signed by the agent DID** — the card must verify against the
   DID it claims; a card that fails signature verification is
   untrusted.
2. **HTTPS only** for production cards; the reference implementation
   enforces this at L2+ conformance.
3. **Registry mirroring:** an agent MAY announce (part 02) with a
   `card_url`; registries MAY fetch and cache cards, re-serving the
   agent's signature (never re-signing).
4. **Card updates:** `updated_at` + re-signature; clients re-fetch on a
   configurable TTL.
5. **Capability prices use decimal strings** (RFC-2119-style precision
   requirement, matching OAP).

### 3.3 Discovery flow

1. Client knows (or resolves) the agent's domain.
2. GET `/.well-known/gap-agent.json` (RFC 8615 well-known URI).
3. Verify agent signature against claimed DID.
4. Optionally verify credentials (RFC-0005) and reputation.
5. Invoke via declared endpoint, under a normal GAP contract.

### 3.4 Conformance requirements

- Serve a self-signed AgentCard at the well-known URI.
- Verify cards before trusting them.
- Registries MAY mirror cards, preserving agent signatures.
- Declare prices as decimal strings.

## 4. Security & privacy considerations

- **Domain compromise:** an attacker controlling the domain controls
  the card — but the DID signature binds the card to the key, so
  domain compromise without key compromise is detected.
- **Downgrade:** clients MUST NOT accept HTTP cards at L2+.
- **Censorship:** well-known discovery is domain-rooted and
  censorship-resistant only where the domain is; registries remain the
  censorship-resistant aggregate.

## 5. Backward compatibility

Additive. Registry discovery unchanged. `card_url` is an optional
announcement field.

## 6. Reference implementation

- `src/agentcard.rs`:
  - `AgentCard { version, agent, capabilities, endpoints, auth,
    autonomy, jurisdictions, credentials, updated_at, sig }`
  - `verify(&self, did: &Did) -> Result<()>`
  - `fetch(url, allow_http: bool) -> Result<AgentCard>` (HTTP client
    behind a trait; in-memory mock for tests)
  - `Registry::mirror_card(card)` — store with agent's signature.
- Tests: self-sign verify, tamper detection, mirroring preserves
  signature, decimal price validation.

## 7. Review notes

- (pending)

## 8. Open questions

- A2A interop: serve the same card as `agent-card.json`?
  Proposal: yes — a `bindings/` doc describing the mapping (v0.3).

## 9. References

- GAP spec part 02 (discovery).
- A2A Agent Card, OAP §6 (manifest), RFC 8615 (well-known URIs).
