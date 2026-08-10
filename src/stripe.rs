//! The Stripe funding rail (RFC-0016 §5.3).
//!
//! Card payments open the market to buyers who hold no crypto, which is
//! most of them. They also bring two problems the on-chain rail does
//! not have, and both are handled here rather than discovered later.
//!
//! **Chargebacks.** A card payment can be reversed weeks after the
//! balance it funded has been spent. That loss is unrecoverable and
//! falls on the operator, not on the protocol - and an attacker can
//! make it a method rather than an accident. The defence is a
//! settlement delay at least as long as the dispute window, so credited
//! funds are not immediately spendable.
//!
//! **Fraud.** Stripe Radar scores every charge. A node that ignores the
//! score is funding an agent economy with stolen cards.
//!
//! The gateway is **disabled unless explicitly enabled**, and refuses to
//! start half-configured. A payment rail that silently accepts money
//! without a webhook secret is worse than one that is switched off.

use crate::amount::Amount;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// How much risk a charge may carry before the node refuses it.
///
/// Stripe's `risk_level` is the coarse signal (`normal`, `elevated`,
/// `highest`); `risk_score` is 0-99. Blocking on level is what a small
/// operator can reason about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    #[default]
    Normal,
    Elevated,
    Highest,
    /// Stripe did not score it (test mode, some payment methods).
    Unknown,
}

impl RiskLevel {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "normal" => RiskLevel::Normal,
            "elevated" => RiskLevel::Elevated,
            "highest" => RiskLevel::Highest,
            _ => RiskLevel::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RiskLevel::Normal => "normal",
            RiskLevel::Elevated => "elevated",
            RiskLevel::Highest => "highest",
            RiskLevel::Unknown => "unknown",
        }
    }

    fn rank(&self) -> u8 {
        match self {
            RiskLevel::Normal => 0,
            // An unscored charge is treated as elevated, not as normal:
            // absence of a signal is not evidence of safety.
            RiskLevel::Unknown | RiskLevel::Elevated => 1,
            RiskLevel::Highest => 2,
        }
    }
}

/// Configuration for the card rail. Absent means switched off.
#[derive(Debug, Clone, Default)]
pub struct StripeGateway {
    pub enabled: bool,
    /// Verifies that a webhook really came from Stripe.
    pub webhook_secret: String,
    /// Refuse at or above this level.
    pub block_at: RiskLevel,
    /// Credited funds are not spendable until this many seconds have
    /// passed, covering the chargeback window.
    pub settlement_delay_seconds: u64,
    /// Above this lifetime total, an agent must have passed identity
    /// verification. Below it, friction would cost more than the fraud.
    pub kyc_threshold: Option<Amount>,
    /// Reject a webhook whose timestamp is older than this, so a
    /// captured request cannot be replayed later.
    pub tolerance_seconds: u64,
}

impl StripeGateway {
    pub fn from_env() -> Self {
        // Disabled unless explicitly switched on. A payment rail that
        // turns itself on because a key happens to be present is how an
        // operator discovers it is taking money it cannot reconcile.
        let enabled = matches!(
            std::env::var("GAP_STRIPE_ENABLED")
                .unwrap_or_default()
                .trim()
                .to_lowercase()
                .as_str(),
            "1" | "true" | "yes"
        );
        Self {
            enabled,
            webhook_secret: std::env::var("GAP_STRIPE_WEBHOOK_SECRET").unwrap_or_default(),
            block_at: std::env::var("GAP_STRIPE_BLOCK_RISK")
                .map(|v| RiskLevel::parse(&v))
                .unwrap_or(RiskLevel::Elevated),
            settlement_delay_seconds: std::env::var("GAP_STRIPE_SETTLEMENT_DELAY_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                // Card disputes can arrive months later; a week is the
                // minimum that is not simply pretending.
                .unwrap_or(604_800),
            kyc_threshold: std::env::var("GAP_STRIPE_KYC_ABOVE")
                .ok()
                .and_then(|v| Amount::parse(&v).ok()),
            tolerance_seconds: std::env::var("GAP_STRIPE_TOLERANCE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
        }
    }

    /// Why this gateway must not be used, if it must not be.
    ///
    /// Returned rather than panicking so an operator sees the reason at
    /// the first request instead of at boot, and so a half-configured
    /// rail never silently accepts money.
    pub fn refusal(&self) -> Option<String> {
        if !self.enabled {
            return Some(
                "the Stripe gateway is disabled on this node; set GAP_STRIPE_ENABLED=1 to \
enable it"
                    .into(),
            );
        }
        if self.webhook_secret.trim().is_empty() {
            return Some(
                "the Stripe gateway is enabled but has no webhook secret: without it any caller \
can credit a balance, so it refuses to run"
                    .into(),
            );
        }
        None
    }

    /// Does this charge clear the risk bar?
    pub fn accepts_risk(&self, level: RiskLevel) -> bool {
        level.rank() < self.block_at.rank()
    }

    /// Does crediting `total_after` oblige the agent to have passed KYC?
    pub fn requires_kyc(&self, total_after: Amount) -> bool {
        self.kyc_threshold
            .map(|t| total_after > t)
            .unwrap_or(false)
    }

    /// When funds credited now become spendable.
    pub fn spendable_at(&self, now: u64) -> u64 {
        now.saturating_add(self.settlement_delay_seconds)
    }
}

/// What a verified Stripe webhook told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripePayment {
    /// Stripe's event id; the idempotency key.
    pub event_id: String,
    /// Which agent this funds, carried in the session metadata.
    pub agent_did: String,
    pub amount: Amount,
    pub currency: String,
    pub risk: RiskLevel,
    pub livemode: bool,
}

/// Verify the `Stripe-Signature` header over the raw request body.
///
/// The scheme is HMAC-SHA256 over `"{timestamp}.{payload}"`. Two details
/// matter and are easy to get wrong:
///
/// - the signed payload is the **raw bytes**, so re-serializing the JSON
///   before checking breaks every signature;
/// - the timestamp must be checked, or a captured webhook can be
///   replayed indefinitely.
pub fn verify_signature(
    payload: &[u8],
    header: &str,
    secret: &str,
    now: u64,
    tolerance: u64,
) -> Result<()> {
    use hmac::{Hmac, Mac};
    if secret.trim().is_empty() {
        return Err(Error::Unauthorized("no webhook secret configured".into()));
    }
    let mut timestamp: Option<u64> = None;
    let mut signatures: Vec<&str> = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => timestamp = v.trim().parse().ok(),
            Some(("v1", v)) => signatures.push(v.trim()),
            _ => {}
        }
    }
    let timestamp =
        timestamp.ok_or_else(|| Error::Unauthorized("signature header has no timestamp".into()))?;
    if now.abs_diff(timestamp) > tolerance {
        return Err(Error::Unauthorized(
            "webhook timestamp outside tolerance: a captured request must not replay".into(),
        ));
    }
    if signatures.is_empty() {
        return Err(Error::Unauthorized("signature header has no v1 entry".into()));
    }

    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| Error::Other("invalid webhook secret".into()))?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    let expected = hex::encode(mac.finalize().into_bytes());

    // Constant-time-ish: compare every candidate fully rather than
    // returning on the first differing byte.
    let ok = signatures
        .iter()
        .any(|s| s.len() == expected.len() && constant_time_eq(s.as_bytes(), expected.as_bytes()));
    if ok {
        Ok(())
    } else {
        Err(Error::Unauthorized("webhook signature does not match".into()))
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Read a payment out of a verified `checkout.session.completed` or
/// `payment_intent.succeeded` event.
///
/// The agent DID travels in the session metadata: Stripe knows nothing
/// about GAP identities, so whoever creates the checkout session must
/// put it there. Without it the payment cannot be attributed and is
/// refused rather than guessed at.
pub fn payment_from_event(event: &serde_json::Value) -> Result<StripePayment> {
    let kind = event
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    if !matches!(
        kind,
        "checkout.session.completed" | "payment_intent.succeeded" | "charge.succeeded"
    ) {
        return Err(Error::Other(format!("event {kind} does not fund anything")));
    }
    let object = event
        .get("data")
        .and_then(|d| d.get("object"))
        .ok_or_else(|| Error::Other("event has no object".into()))?;

    // Only a paid session funds a balance. An unpaid one is a customer
    // who reached the page and left.
    if let Some(status) = object.get("payment_status").and_then(|v| v.as_str()) {
        if status != "paid" {
            return Err(Error::EscrowViolation(format!(
                "payment_status is {status}, not paid"
            )));
        }
    }

    let agent_did = object
        .get("metadata")
        .and_then(|m| m.get("agent_did"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            Error::Other(
                "no agent_did in the session metadata: an unattributable payment is refused, \
not guessed at"
                    .into(),
            )
        })?
        .to_string();

    // Stripe reports minor units of the presentment currency.
    let minor = object
        .get("amount_total")
        .or_else(|| object.get("amount_received"))
        .or_else(|| object.get("amount"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| Error::Other("event carries no amount".into()))?;
    let currency = object
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("usd")
        .to_uppercase();

    let risk = object
        .get("outcome")
        .and_then(|o| o.get("risk_level"))
        .and_then(|v| v.as_str())
        .map(RiskLevel::parse)
        .unwrap_or(RiskLevel::Unknown);

    Ok(StripePayment {
        event_id: event
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        agent_did,
        // Stripe uses two decimals for these currencies; Amount tracks
        // six, so scale rather than reinterpret.
        amount: Amount::from_minor(minor as u128 * 10_000),
        currency,
        risk,
        livemode: event
            .get("livemode")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn signed(payload: &[u8], secret: &str, ts: u64) -> String {
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(ts.to_string().as_bytes());
        mac.update(b".");
        mac.update(payload);
        format!("t={ts},v1={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn the_gateway_is_off_until_it_is_switched_on() {
        // A payment rail that turns itself on because a key happens to
        // be present is how an operator discovers it is taking money it
        // cannot reconcile.
        let g = StripeGateway::default();
        assert!(g.refusal().unwrap().contains("disabled"));
    }

    #[test]
    fn enabled_without_a_webhook_secret_refuses_to_run() {
        let g = StripeGateway {
            enabled: true,
            ..Default::default()
        };
        let why = g.refusal().unwrap();
        assert!(why.contains("no webhook secret"), "{why}");
        assert!(why.contains("any caller can credit a balance"), "{why}");
    }

    #[test]
    fn a_valid_signature_passes_and_a_tampered_body_does_not() {
        let secret = "whsec_test";
        let body = br#"{"id":"evt_1","type":"charge.succeeded"}"#;
        let header = signed(body, secret, 1_000);
        verify_signature(body, &header, secret, 1_000, 300).unwrap();

        // One byte changed in the body invalidates it.
        let tampered = br#"{"id":"evt_2","type":"charge.succeeded"}"#;
        assert!(verify_signature(tampered, &header, secret, 1_000, 300).is_err());
        // A different secret does too.
        assert!(verify_signature(body, &header, "whsec_other", 1_000, 300).is_err());
    }

    #[test]
    fn an_old_webhook_cannot_be_replayed() {
        // Without a timestamp check a captured request funds a balance
        // again every time it is sent.
        let secret = "whsec_test";
        let body = br#"{"id":"evt_1"}"#;
        let header = signed(body, secret, 1_000);
        let err = verify_signature(body, &header, secret, 100_000, 300).unwrap_err();
        assert!(err.to_string().contains("outside tolerance"), "{err}");
    }

    #[test]
    fn a_header_without_a_signature_is_refused() {
        assert!(verify_signature(b"{}", "t=1000", "whsec", 1_000, 300).is_err());
        assert!(verify_signature(b"{}", "v1=abc", "whsec", 1_000, 300).is_err());
        // ...and so is a node with no secret at all.
        assert!(verify_signature(b"{}", "t=1000,v1=abc", "", 1_000, 300).is_err());
    }

    #[test]
    fn an_unattributable_payment_is_refused_rather_than_guessed_at() {
        // Stripe knows nothing about GAP identities. Without the DID in
        // the metadata there is no honest way to decide whose money it
        // is.
        let e = json!({
            "id": "evt_1", "type": "checkout.session.completed",
            "data": { "object": { "amount_total": 5000, "currency": "usd",
                                  "payment_status": "paid" } }
        });
        let err = payment_from_event(&e).unwrap_err().to_string();
        assert!(err.contains("no agent_did"), "{err}");
    }

    #[test]
    fn an_unpaid_session_funds_nothing() {
        let e = json!({
            "id": "evt_1", "type": "checkout.session.completed",
            "data": { "object": { "amount_total": 5000, "currency": "usd",
                                  "payment_status": "unpaid",
                                  "metadata": { "agent_did": "did:gap:aaa" } } }
        });
        assert!(payment_from_event(&e).is_err());
    }

    #[test]
    fn a_paid_session_is_read_with_its_risk_level() {
        let e = json!({
            "id": "evt_9", "type": "checkout.session.completed", "livemode": true,
            "data": { "object": {
                "amount_total": 2500, "currency": "eur", "payment_status": "paid",
                "metadata": { "agent_did": "did:gap:aaa" },
                "outcome": { "risk_level": "elevated" }
            }}
        });
        let p = payment_from_event(&e).unwrap();
        assert_eq!(p.agent_did, "did:gap:aaa");
        assert_eq!(p.amount.to_string_decimal(), "25.000000");
        assert_eq!(p.currency, "EUR");
        assert_eq!(p.risk, RiskLevel::Elevated);
        assert!(p.livemode);
        assert_eq!(p.event_id, "evt_9");
    }

    #[test]
    fn an_unscored_charge_is_treated_as_elevated_not_as_safe() {
        // Absence of a signal is not evidence of safety.
        let g = StripeGateway {
            enabled: true,
            block_at: RiskLevel::Elevated,
            ..Default::default()
        };
        assert!(g.accepts_risk(RiskLevel::Normal));
        assert!(!g.accepts_risk(RiskLevel::Unknown));
        assert!(!g.accepts_risk(RiskLevel::Elevated));
        assert!(!g.accepts_risk(RiskLevel::Highest));
    }

    #[test]
    fn a_permissive_operator_can_accept_elevated_but_not_the_top() {
        let g = StripeGateway {
            enabled: true,
            block_at: RiskLevel::Highest,
            ..Default::default()
        };
        assert!(g.accepts_risk(RiskLevel::Elevated));
        assert!(!g.accepts_risk(RiskLevel::Highest));
    }

    #[test]
    fn kyc_applies_above_the_threshold_only() {
        let g = StripeGateway {
            enabled: true,
            kyc_threshold: Amount::parse("100.00").ok(),
            ..Default::default()
        };
        assert!(!g.requires_kyc(Amount::parse("99.99").unwrap()));
        assert!(!g.requires_kyc(Amount::parse("100.00").unwrap()));
        assert!(g.requires_kyc(Amount::parse("100.000001").unwrap()));
        // With no threshold configured, nothing is gated.
        assert!(!StripeGateway::default().requires_kyc(Amount::parse("1000000").unwrap()));
    }

    #[test]
    fn card_funds_are_not_spendable_until_the_dispute_window_passes() {
        // A chargeback after the balance has been spent is an
        // unrecoverable loss, and an attacker can make it a method.
        let g = StripeGateway {
            enabled: true,
            settlement_delay_seconds: 604_800,
            ..Default::default()
        };
        assert_eq!(g.spendable_at(1_000), 605_800);
    }
}
