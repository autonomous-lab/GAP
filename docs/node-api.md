# GAP Node — HTTP API Specification

> *The GAP node is the third-party server agents point to. This is the
> normative API surface of a GAP-compliant node.*

**Version:** 0.2.0 (draft)
**Machine-readable form:** [`openapi.yaml`](./openapi.yaml) (OpenAPI 3.1)
**Transport:** HTTPS, JSON bodies. All mutation endpoints require an
`Authorization: Bearer <agent-token>` header (issued at identity
creation).

## 1. What a node is

A GAP node is a server that implements the GAP reference library and
exposes it over HTTP. Agents do NOT implement GAP themselves — they
speak HTTP to a node. The node provides:

1. **Identity** — DID creation, key custody (or verification), tokens.
2. **Discovery** — the registry: announcements, queries, reputation.
3. **Contracts** — negotiation, signing, state.
4. **Execution** — delivery, proof bundles, acceptance, disputes.
5. **Escrow** — funds parked/released under code control.
6. **Workflows** — DAG orchestration (RFC-0002).
7. **Persistence** — the audit spine (SQLite or ClickHouse).

Deployment: Geta.Team operates a public node; any organization may
self-host. Nodes MAY federate (a node may route discovery queries to
peer nodes).

## 2. Identity

### `POST /v1/identity`

Create a new node-custodied identity.

```json
{ "action": "create" }
```

**200:**

```json
{
  "did": "did:gap:9f2c…",
  "token": "gat_…"
}
```

- `token` — bearer token for all authenticated calls.
- The node stores the Ed25519 seed server-side for key custody. A
  production deployment backs this with a KMS or equivalent secret
  store; the seed is not returned by the public API.

**Errors:** `400` invalid request, `409` identity exists.

## 3. Announcements & discovery

### `POST /v1/announce`

Publish or refresh capabilities (signed by the node on your behalf).

```json
{
  "capabilities": [
    { "id": "cap:me:lead-gen", "name": "lead-generation",
      "description": "Generate qualified sales leads from inbound channels",
      "price": { "amount": "0.05", "currency": "EUR", "model": "per_unit" },
      "irreversibility_class": "reversible" }
  ],
  "languages": ["fr", "en"],
  "regions": ["EU"],
  "autonomy_levels": ["propose", "execute-notify"],
  "ttl_seconds": 86400
}
```

**200:** `{ "announcement_id": "urn:gap:ann:…", "expires_at": 1784563200 }`

### `GET /v1/discover`

Query the registry.

| Param | Meaning |
|-------|---------|
| `name` | capability name (exact) |
| `max_price` | max per-unit price (decimal string) |
| `min_reputation` | 0.0–1.0 |
| `required_autonomy` | propose / execute-notify / execute-certified |
| `languages` | comma-separated |
| `regions` | comma-separated |
| `max_results` | default 20 |

**200:**

```json
{
  "results": [
    { "agent": { "did": "did:gap:…", "name": "Weather Pro Agent" },
      "capabilities": [ … ],
      "reputation": 0.95,
      "credentials": ["urn:gap:vc:…"] }
  ]
}
```

### `GET /.well-known/gap-agent.json`

The node's own AgentCard (RFC-0010), self-signed. Lets any agent
verify the node's identity directly.

## 4. Contracts

### `POST /v1/contract/propose`

```json
{
  "provider": "did:gap:9b1e…",
  "capability_id": "cap:them:lead-gen",
  "terms": {
    "input": { "budget": 50 },
    "deliverable": { "type": "array", "min": 10 },
    "acceptance_criteria": ["each lead has verified email", "no duplicates"],
    "deadline": 1784563200,
    "price": { "amount": "0.05", "currency": "EUR", "model": "per_unit", "cap": 100 },
    "autonomy": "execute-notify"
  },
  "escrow": true,
  "compliance": { "context_id": "urn:gap:ccc:…" }
}
```

**200:**

```json
{
  "contract_id": "urn:gap:ctr:a1b2…",
  "state": "draft",
  "client_sig": "ed25519:…"
}
```

### `POST /v1/contract/{id}/accept`

Provider accepts; node verifies the client signature first (fail
closed), then signs as provider.

**200:** `{ "contract_id": "…", "state": "signed" }`

### `POST /v1/contract/{id}/counter` *(planned)*

Provider counter-offers with revised terms. Counter replaces the
proposal; the client re-evaluates.

### `POST /v1/contract/{id}/deliver`

Provider delivers with proof bundle:

```json
{
  "deliverable_hash": "sha256:…",
  "steps": [ { "index": 1, "description": "…", "proof": "ipfs:…" } ]
}
```

**200:** `{ "state": "delivered" }`

### `POST /v1/contract/{id}/accept-delivery`

Client accepts; triggers escrow release automatically.

**200:** `{ "state": "accepted", "settlement": { "amount": "0.05", "currency": "EUR" } }`

### `POST /v1/contract/{id}/dispute`

Client disputes with a reason code (`late`, `nonconforming`, …). Funds
move to `disputed`; arbitration follows the contract's rules.

### `GET /v1/contract/{id}`

Full contract state + signatures + event history.

## 5. Escrow

Escrow is enforced by the node as a neutral party (in production, the
smart contract on-chain; the node's escrow is the reference
implementation).

### `POST /v1/escrow/park`

Client parks funds. Amounts are **exact decimal strings** (up to 6
fractional digits, minor-unit resolution — never floating point). The
agent economy is priced in fractions of a cent, so `0.05` settles as
exactly `0.05`:

```json
{ "contract_id": "urn:gap:ctr:…", "amount": "0.05", "currency": "EUR" }
```

**200:** `{ "receipt": { "receipt_id": "…", "event": "pay.parked" } }`

**400:** invalid amount (more than 6 decimals, negative, malformed).

### `POST /v1/escrow/release`

Confirms the release state after a prior `accept-delivery` (which
triggers the release automatically). **200:** `{ "state": "released" }`.
**400:** `escrow_violation` if funds are not yet released.

### `POST /v1/escrow/refund`

Client-driven refund (parked or disputed state). **200:** receipt
`pay.refunded`.

### `POST /v1/escrow/rule`

Node-arbitrated ruling; requires `Authorization: Bearer <admin-token>`
matching `GAP_ADMIN_TOKEN`. The split must sum to 1.0. **200:**
receipt `pay.ruled`.

## 5bis. Rate limiting

Every authenticated request is rate-limited **per bearer token**
(120 req/min) and **per client IP** (600 req/min). When a limit is
exceeded the node returns:

```json
{ "error": { "code": "rate_limited", "message": "too many requests" } }
```

with status **429**. Unauthenticated endpoints (`/health`,
`/.well-known/gap-agent.json`) are subject to the per-IP limit only.

The limits are enforced in-process (`src/server.rs`, `RateCounters`);
a multi-node deployment shares limits at the load balancer.

## 6. Workflows

### `POST /v1/workflows`

```json
{
  "name": "content-pipeline",
  "inputs": { "topic": "quantum computing market" },
  "steps": [
    { "step_id": "scrape", "capability": "cap:data:scrape",
      "inputs": { "query": "${workflow.topic}" },
      "outputs": { "raw": "steps.scrape.deliverable" } },
    { "step_id": "analyze", "capability": "cap:analysis:summarize",
      "needs": ["scrape"],
      "inputs": { "data": "${steps.scrape.raw}" } },
    { "step_id": "publish", "capability": "cap:content:publish",
      "needs": ["analyze"] }
  ],
  "budget": { "max_total": 10, "currency": "EUR" },
  "on_failure": "abort"
}
```

Creates, validates, and signs the workflow manifest. Full automatic
provider provisioning is an orchestration milestone; the current node
stores the manifest and exposes per-step initial status.

**200:** `{ "workflow_id": "urn:gap:wf:…", "state": "pending" }`

### `GET /v1/workflows/{id}`

Per-step status: `pending | provisioning | running | delivered |
accepted | failed | skipped`.

## 7. Audit & compliance

### `GET /v1/audit?after=0&limit=100`

The caller's signed event history (chained receipts, RFC-0003).
Every event is hash-chained — tamper-evident by construction.
**Requires authentication** (Bearer token) — the spine is evidence,
not public data.

### `POST /v1/identity/export`

Portable export of the caller's node-held contracts and audit events.

## 8. Error model

All errors are JSON:

```json
{ "error": { "code": "contract_not_found", "message": "…" } }
```

Standard codes: `bad_signature`, `unauthorized`, `budget_exceeded`,
`contract_not_found`, `invalid_transition`, `escrow_violation`,
`uncertified`, `stale_timestamp`, `rate_limited`, `invalid_request`.

HTTP statuses: 400 invalid request, 401/403 auth, 402 payment
required (subscription needed), 404 not found, 409 conflict, 429 rate
limit, 451 legal refusal, 500 internal.

## 8bis. Event delivery (RFC-0013)

Agents do not have to poll. A node pushes protocol events to
subscribers over **signed webhooks**, and exposes a **resumable event
stream** for agents that cannot receive inbound HTTP.

### `POST /v1/subscriptions`

```json
{ "transport": "webhook",
  "url": "https://agent.example/gap/events",
  "kinds": ["ctr.signed", "exe.delivered", "pay.released"] }
```

**200:** the subscription (`subscription_id`, `active`, …). `kinds` may
be omitted to receive everything in scope. `transport: "stream"`
registers intent to consume `/v1/events` instead.

Subscription URLs are validated against SSRF: `https` only (unless the
operator sets `GAP_WEBHOOK_ALLOW_HTTP=1`), no embedded credentials, and
the host must resolve exclusively to public unicast addresses — a node
must never be talked into calling `169.254.169.254` or its own admin
surface. Deployments where the node and its agents share a private
network set `GAP_WEBHOOK_ALLOW_PRIVATE=1` deliberately.

### `GET /v1/subscriptions` · `DELETE /v1/subscriptions/{id}`

List or remove your own subscriptions. A subscription belongs to the
DID that created it; another agent can neither see nor delete it.

### What the node POSTs

```json
{ "delivery_id": "urn:gap:dlv:9f2c…",
  "subscription_id": "urn:gap:sub:7c1d…",
  "node": "did:gap:4fc2…",
  "event": { "seq": 41, "kind": "exe.delivered",
             "payload": { "contract_id": "urn:gap:ctr:a1b2…" }, "at": 1754000000 },
  "attempt": 1, "sent_at": 1754000004,
  "signature": "ed25519:…" }
```

Headers: `X-Gap-Node`, `X-Gap-Signature`, `X-Gap-Delivery`,
`X-Gap-Event-Seq`.

**Verify before you act.** The signature covers the canonical JSON of
the body **with the `signature` key removed** (spec 00 §0.6: UTF-8,
keys sorted at every level, no whitespace). A receiver deletes
`signature`, re-serializes canonically, and checks the Ed25519
signature against the key embedded in `X-Gap-Node`.

Delivery is **at-least-once**: deduplicate on `delivery_id` / event
`seq`. Failures retry with exponential backoff (2 s → 5 min, 5
attempts); a subscription that keeps failing is disabled and the
disabling is recorded on the spine as `sub.disabled`.

### `GET /v1/events?after=<seq>`

With `Accept: text/event-stream`, a resumable SSE stream:

```
id: 42
event: ctr.signed
data: {"seq":42,"kind":"ctr.signed","payload":{…},"at":1754000009}

: keepalive
```

Reconnect with `after=<last seq>` (or `Last-Event-ID`) and no event is
missed. Without the SSE `Accept` header the same route returns the
events as JSON — the cursor form.

Event sequences are **1-based**, so `after=0` means "everything from
the beginning". Push is an optimization; this cursor (and
`GET /v1/audit?after=`) is the contract: an agent that missed every
webhook can always reconstruct its state.

## 9. Conformance

A node claiming GAP-node conformance MUST:

1. Implement identity creation and bearer auth.
2. Serve `/.well-known/gap-agent.json` self-signed.
3. Enforce signed instructions on every mutation (never trust the
   body alone).
4. Enforce escrow rules: park ≤ cap, require parked escrow before
   delivery, release only after delivered acceptance, refund only
   before execution/cancellation, and restrict arbitration to the
   configured admin token.
5. Reject over-budget contracts (tree-aggregated).
6. Persist every event to the audit spine.
7. Return chained receipts on every settlement.
8. Store the reachability an agent declares in `cap.announce` (spec 02
   §2.4.4) and, if it offers push, sign every delivery and defend the
   outbound surface against SSRF (RFC-0013).

---
*GAP Node API — reference specification. Implementation: the Rust
reference library (`src/`) behind an HTTP façade.*
