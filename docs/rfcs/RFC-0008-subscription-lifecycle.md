# RFC-0008: Subscription Lifecycle

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Commercial
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

GAP lists `subscription` as a price model but defines no lifecycle.
This RFC specifies it end-to-end: initiation (HTTP 402), consent
recording, subscription tokens, renewal, price-change notice, and
termination — with principal budget enforcement at every step.

## 2. Motivation

OAP §12 standardizes the subscription lifecycle including 402
responses and consent receipts; our spec 07 tokenomics references
subscription pricing. A provider cannot actually *sell* a
subscription to an agent today — the agent has no way to subscribe
autonomously within budget, and the provider has no token to
authenticate it. This RFC closes the commercial gap.

## 3. Specification

### 3.1 Terminology

| Term | Definition |
|------|------------|
| **Subscription** | Recurring authorization to invoke a capability. |
| **Subscription token** | Signed artifact binding subscriber, scope, tier, price. |
| **Consent receipt** | Record of principal authorization (or policy-based auto-consent). |

### 3.2 Lifecycle state machine

```
  none ─► pending_consent ─► active ─► renewing ─► active
   │            │              │          │
   ▼            ▼              ▼          ▼
 declined     expired      cancelled   price_changed
```

### 3.3 Initiation

When an agent invokes an action requiring a subscription:

```
HTTP 402 Payment Required
{
  "error": "subscription_required",
  "checkout_url": "https://provider.example/gap/subscribe/…",
  "price_options": [ { "tier": "pro", "amount": "9.99",
                       "currency": "EUR", "interval": "month" } ],
  "user_consent_required": true,
  "trial_available": true
}
```

### 3.4 Consent (normative)

1. The agent evaluates principal policy (RFC-0004 L4): if auto-consent
   is permitted within budget, record a **consent receipt** and proceed.
2. Otherwise, request explicit consent via the registered consent
   channel; no subscription starts without consent.
3. Consent receipts are signed and chained (RFC-0003).

### 3.5 Subscription token

```json
{
  "subscription_id": "urn:gap:sub:9d2e",
  "subscriber": "did:gap:9f2c…",
  "provider": "did:gap:9b1e…",
  "capability": "cap:data:api",
  "tier": "pro",
  "price_hash": "sha256:…",
  "period_start": "2026-08-08T00:00:00Z",
  "period_end": "2026-09-08T00:00:00Z",
  "renewable": true,
  "provider_sig": "ed25519:…"
}
```

- Bound to subscriber DID, tier, price hash, and period.
- Presented on each invocation; provider verifies signature + period.
- Budget enforcement: renewal MUST re-check the subscriber's remaining
  budget (tree-aggregated, RFC-0007) and fail if exceeded.

### 3.6 Price changes & termination

- Price changes affecting active subscriptions MUST be announced ≥ 30
  days ahead via the billing endpoint; the old price holds until the
  current period ends.
- Termination MUST be available through both provider and subscriber
  interfaces with no friction asymmetry.
- Non-payment or budget exhaustion → grace period (3 days) → suspend →
  terminate; suspension and termination produce chained receipts.

### 3.7 Conformance requirements

- Implement initiation (402) and consent flow.
- Issue/verify subscription tokens.
- Enforce budget at renewal.
- Honor 30-day price-change notice and symmetric termination.

## 4. Security & privacy considerations

- **Token theft:** tokens are bound to subscriber DID; presenting a
  stolen token fails signature verification.
- **Runaway renewals:** budget checks + grace period + principal
  kill-switch (RFC-0004 L4) contain cost.
- **Consent laundering:** no auto-consent above principal-declared
  thresholds.

## 5. Backward compatibility

Additive: new kinds (`sub.request`, `sub.consent`, `sub.token`,
`sub.renew`, `sub.cancel`), no changes to existing messages.

## 6. Reference implementation

- `src/subscription.rs`:
  - `Subscription { id, subscriber, provider, capability, tier,
    price_hash, period_start, period_end, renewable, sig }`
  - `SubscriptionManager { start(), renew(), cancel(), suspend() }`
  - `consent_receipt(principal_policy, budget) -> Result<ConsentReceipt>`
  - Tests: lifecycle, renewal budget fail, price-change notice,
    consent denial, token verify.

## 7. Review notes

- (pending)

## 8. Open questions

- Trials: `trial_available` — proposal: trial periods are one-shot
  subscriptions with zero price and a trial flag.

## 9. References

- GAP spec part 03 (price models), spec 07 (tokenomics).
- OAP §12 (subscription lifecycle), RFC-0004 (policy), RFC-0003
  (receipt chains).
