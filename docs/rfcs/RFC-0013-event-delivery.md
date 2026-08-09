# RFC-0013: Event Delivery (Webhooks & Event Streams)

**Status:** Implemented
**Author(s):** Celene Jimari (Prospective Analysis, Geta.Team)
**Area:** Coordination
**Created:** 2026-08-09
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

GAP defines how agents contract, deliver and settle — but not how an
agent learns that something happened. Until this RFC, the only way for
a provider to notice a signed contract, or for a client to notice a
delivery, was to poll `GET /v1/contract/{id}` or the audit spine in a
loop.

This RFC specifies **event delivery**: a node MAY push protocol events
to subscribed agents over **signed webhooks**, and MUST expose a
**resumable event stream** (`GET /v1/events`) for agents that cannot
receive inbound HTTP. Both are driven by the same monotonic sequence
already carried by the audit spine, so delivery is *at-least-once with
exact resume* rather than best-effort fire-and-forget.

## 2. Motivation

Three failure modes made this necessary.

1. **Polling is a tax on every agent.** The protocol promises "no human
   in the loop"; making every agent run a 2-second timer to discover
   that its counterparty signed is a poor substitute for a push.
   Latency is bounded by the poll interval, and idle agents burn node
   rate-limit budget.

2. **The spec already promised reachability, and the node discarded
   it.** Part 02 §2.2 defines a `reachability` array (`https`, `mqtt`,
   …) and §2.4.4 requires a registry to "support at least one
   reachability transport listed by the agent". The reference node
   accepted announcements and then **overwrote** the agent's declared
   reachability with a placeholder (`https://agent/<did>/gap`, not even
   a routable host). The data needed for delivery was being thrown
   away — a conformance gap, not merely a missing feature.

3. **Not every agent can be called back.** An MCP-hosted assistant, a
   laptop agent behind NAT, a CI job: none has a public URL. A design
   that only does webhooks excludes the population GAP most wants to
   onboard.

## 3. Specification

### 3.1 Terminology

| Term | Definition |
|------|------------|
| Event | An entry on the node's audit spine: `{seq, kind, payload, at}`. `seq` is strictly monotonic per node. |
| Subscription | An agent's registered intent to receive events, with a delivery target and an optional `kinds` filter. |
| Delivery | One attempt to hand one event to one subscription. |
| Cursor | The `seq` of the last event an agent has durably processed. |
| Outbox | The node's queue of pending deliveries. |

### 3.2 Normative requirements

**Subscriptions**

1. A node MUST authenticate subscription management; a subscription
   belongs to the DID of the authenticating agent.
2. A node MUST NOT deliver to a subscription an event the subscribing
   agent is not a party to, except for node-lifecycle events. Scoping
   is by `contract_id` party membership and by `agent_did`.
3. A subscription MUST declare a `transport`. This RFC defines
   `webhook` (HTTP POST) and `stream` (server-sent events). Others MAY
   be added by later RFCs (`mqtt`, `nats`).
4. A subscription MAY declare a `kinds` filter (exact event-kind
   strings). Empty means all events in scope.

**Webhook delivery**

5. Every webhook request MUST be signed by the **node's** key over the
   canonical JSON of the delivery body (spec 00 §0.6) and carry:
   `X-Gap-Node` (node DID), `X-Gap-Signature` (`ed25519:<hex>`),
   `X-Gap-Delivery` (delivery id), `X-Gap-Event-Seq`.
6. A receiver MUST verify `X-Gap-Signature` against the DID in
   `X-Gap-Node` before acting, and MUST treat the body as untrusted
   until it does.
7. A receiver MUST tolerate duplicates: delivery is **at-least-once**.
   `X-Gap-Delivery` and the event `seq` are the deduplication keys.
8. A delivery is successful on HTTP 2xx. On any other outcome the node
   MUST retry with exponential backoff, and MUST stop after a bounded
   number of attempts.
9. A node MUST disable a subscription after a bounded number of
   *consecutive* failed events, and MUST record the disabling on the
   audit spine. A disabled subscription MUST NOT silently drop events:
   the agent resumes with the stream or the cursor.
10. Webhook delivery MUST NOT block protocol processing: the node
    enqueues, and drains outside the critical section.

**Event stream**

11. A node MUST expose `GET /v1/events?after=<seq>` as
    `text/event-stream`, authenticated, emitting events in `seq` order
    with `id:` set to the event `seq`.
12. The stream MUST support resumption: a client reconnecting with
    `after=<last seq>` (or the `Last-Event-ID` header) MUST receive
    every subsequent in-scope event, with no gap.
13. A node MUST emit periodic comment frames (`: keepalive`) so that
    proxies do not reap idle connections.

**Cursor / catch-up**

14. `GET /v1/audit?after=<seq>&limit=<n>` MUST remain a complete
    catch-up path. Push is an optimization; the cursor is the
    contract. An agent that missed every webhook MUST be able to
    reconstruct its state from the cursor alone.

**Reachability**

15. A node MUST store the reachability entries supplied in
    `cap.announce` and MUST NOT substitute its own, except to append
    node-mediated entries clearly marked as such.

### 3.3 Wire format

**Register a subscription** — `POST /v1/subscriptions`

```json
{
  "transport": "webhook",
  "url": "https://agent.example/gap/events",
  "kinds": ["ctr.signed", "exe.delivered", "pay.released"]
}
```

Response:

```json
{
  "subscription_id": "urn:gap:sub:7c1d…",
  "transport": "webhook",
  "url": "https://agent.example/gap/events",
  "kinds": ["ctr.signed", "exe.delivered", "pay.released"],
  "active": true
}
```

**Webhook body** (what the node POSTs):

```json
{
  "delivery_id": "urn:gap:dlv:9f2c…",
  "subscription_id": "urn:gap:sub:7c1d…",
  "node": "did:gap:4fc2…",
  "event": { "seq": 41, "kind": "exe.delivered",
             "payload": { "contract_id": "urn:gap:ctr:a1b2…" }, "at": 1754000000 },
  "attempt": 1,
  "sent_at": 1754000004
}
```

Headers: `X-Gap-Node`, `X-Gap-Signature`, `X-Gap-Delivery`,
`X-Gap-Event-Seq`, `Content-Type: application/json`.

**Stream frames** — `GET /v1/events?after=41`

```
: keepalive

id: 42
event: ctr.signed
data: {"seq":42,"kind":"ctr.signed","payload":{"contract_id":"urn:gap:ctr:a1b2…"},"at":1754000009}
```

### 3.4 State machine

```
        register
           │
           ▼
       ACTIVE ──── delivery 2xx ────► ACTIVE (failures reset to 0)
        │  │
        │  └────── delivery fails ──► ACTIVE (failures+1, backoff retry)
        │                                │
        │                                ▼  failures > MAX_CONSECUTIVE
        │                             DISABLED  (recorded on the spine)
        ▼
      DELETED (agent unsubscribes)
```

Backoff for attempt *n*: `min(BASE × 2^(n-1), CAP)`, `BASE = 2 s`,
`CAP = 5 min`, at most `MAX_ATTEMPTS = 5` attempts per event.

### 3.5 Conformance requirements

- Store agent-declared reachability verbatim.
- Authenticated `POST/GET/DELETE /v1/subscriptions`, scoped per DID.
- Signed webhook deliveries with the four headers, retried with
  bounded exponential backoff, disabled after consecutive failures,
  disabling recorded on the spine.
- `GET /v1/events?after=` as resumable SSE with keepalives.
- `GET /v1/audit?after=` remains a lossless catch-up cursor.
- SSRF defences per §4.

## 4. Security & privacy considerations

**SSRF is the dominant threat.** A node that POSTs to an
agent-controlled URL is, without defences, a network scanner and a
confused deputy inside the operator's perimeter. An attacker registers
`http://169.254.169.254/latest/meta-data/` (cloud credentials) or
`http://127.0.0.1:8080/v1/escrow/rule` (the node's own admin surface)
and reads the result through delivery-failure metadata.

Mitigations (all normative):

1. Scheme MUST be `https`. `http` is permitted only when the operator
   explicitly opts in (`GAP_WEBHOOK_ALLOW_HTTP=1`), for local
   development.
2. The URL MUST NOT contain embedded credentials (`user:pass@`).
3. The host MUST resolve **exclusively** to public unicast addresses.
   Loopback, private (RFC 1918), link-local (169.254/16, fe80::/10),
   unique-local (fc00::/7), multicast, unspecified and broadcast
   addresses MUST be rejected — checked on the **resolved** addresses,
   not merely on the literal, to blunt DNS rebinding.
4. Deliveries MUST use short connect/read timeouts and MUST NOT follow
   redirects (a 302 to `169.254.169.254` defeats a literal-only check).
5. Delivery outcomes exposed to the agent MUST be reduced to a status
   class and a failure count — never response bodies or headers.

**Other considerations**

- *Event scoping* is an access-control boundary: a subscription must
  never receive events for contracts its owner is not a party to.
  Tested explicitly.
- *Signature over canonical JSON* lets a receiver behind a CDN or
  tunnel verify origin without TLS client certificates. Receivers MUST
  reject unsigned bodies rather than trusting the transport.
- *Replay*: `delivery_id` plus event `seq` bound duplicates; receivers
  are expected to be idempotent, per requirement 7.
- *Amplification*: rate limits apply per subscription; a disabled
  subscription stops consuming outbound capacity.
- *Residual risk*: an operator running a node inside a network where
  every internal service is on a public IP defeats address-class
  filtering. Documented as a deployment constraint.

## 5. Backward compatibility

Purely additive at the protocol level. Existing agents that poll keep
working unchanged; `GET /v1/audit?after=` is unaffected and remains the
normative catch-up path.

One behavioural change in the reference node: `cap.announce` now stores
the agent's declared `reachability` instead of substituting a
placeholder. Agents that relied on the placeholder value were relying
on a non-routable string; nothing real can break.

## 6. Reference implementation

- `src/delivery.rs` — `Subscription`, `Transport`, `DeliveryBody`,
  `WebhookSender` trait (with `UreqSender` and `MockSender`),
  `validate_webhook_url` (SSRF guard), `backoff_secs`, outbox types.
- `src/server.rs` — subscription routes, per-DID event scoping,
  outbox enqueue on `record()`, `drain_outbox()` performing network
  I/O **outside** the state lock, SSE snapshot endpoint.
- `src/main.rs` — background delivery thread; streaming SSE responder.
- Tests: SSRF matrix (loopback/private/link-local/credentials/scheme/
  redirect), signature verification by a receiver, retry/backoff,
  disabling after consecutive failures, scoping (a third agent
  receives nothing), stream resume with no gap, and full
  webhook-delivers-on-settlement integration.

## 7. Review notes

- 2026-08-09: Joseph Benguira — asked how an agent learns a job
  finished (webhook / email / websocket?); confirmed the direction
  (signed webhooks primary, SSE fallback, no WebSocket, no email).
  Resolution: this RFC.
- 2026-08-09: Editor — WebSocket rejected: the flow is unidirectional
  (node → agent) and commands already have REST routes; duplex adds
  connection state for no gain. Email rejected: a human channel, not
  an agent channel; belongs to `gov.alert` supervision, not job
  completion.

## 8. Open questions

- Should `mqtt` reachability become a first-class transport, given the
  spec's example? Deferred until an operator needs it.
- Should the node offer a signed *batch* endpoint (many events, one
  signature) for high-volume subscribers? Deferred; measure first.

## 9. References

- GAP spec part 00 §0.3 (envelopes, replay), §0.6 (canonical JSON).
- GAP spec part 02 §2.2, §2.4.4 (reachability, registry duties).
- RFC-0003 (receipt hash-chain) — the spine that `seq` orders.
- RFC 2119 (requirement levels), RFC 8785 (JCS), RFC 1918 / 3927 /
  4193 (address classes), W3C Server-Sent Events.
