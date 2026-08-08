//! Subscription lifecycle (RFC-0008).
//!
//! End-to-end subscriptions: initiation (HTTP 402), consent recording,
//! subscription tokens, renewal with budget enforcement, price-change
//! notice, and symmetric termination.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use crate::message::now_unix;
use serde::{Deserialize, Serialize};

/// Subscription lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubscriptionState {
    #[default]
    None,
    PendingConsent,
    Active,
    Renewing,
    Suspended,
    Cancelled,
    Expired,
    Declined,
}

/// Parameters for issuing a subscription (avoids a 9-arg function).
#[derive(Debug, Clone)]
pub struct IssueParams {
    pub subscriber: Did,
    pub capability: String,
    pub tier: String,
    pub price_hash: String,
    pub period_start: u64,
    pub period_end: u64,
    pub renewable: bool,
    pub trial: bool,
}

/// A signed subscription token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub subscription_id: String,
    pub subscriber: Did,
    pub provider: Did,
    pub capability: String,
    pub tier: String,
    pub price_hash: String,
    pub period_start: u64,
    pub period_end: u64,
    pub renewable: bool,
    #[serde(default)]
    pub trial: bool,
    #[serde(default)]
    pub provider_sig: Option<String>,
    #[serde(skip)]
    pub state: SubscriptionState,
}

impl Subscription {
    /// Issue a subscription token after consent.
    pub fn issue(provider: &AgentIdentity, params: IssueParams) -> Self {
        let mut s = Self {
            subscription_id: crate::new_id("sub"),
            subscriber: params.subscriber,
            provider: provider.did().clone(),
            capability: params.capability,
            tier: params.tier,
            price_hash: params.price_hash,
            period_start: params.period_start,
            period_end: params.period_end,
            renewable: params.renewable,
            trial: params.trial,
            provider_sig: None,
            state: SubscriptionState::Active,
        };
        s.resign(provider);
        s
    }

    /// Re-sign after mutation.
    pub fn resign(&mut self, provider: &AgentIdentity) {
        self.provider_sig = None;
        let canonical = self.canonical_bytes();
        self.provider_sig = Some(provider.sign(&canonical).to_hex());
    }

    /// Verify provider signature and current period.
    pub fn verify(&self, now: u64) -> Result<()> {
        let sig_hex = self.provider_sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.provider,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )?;
        if now < self.period_start {
            return Err(Error::Other("subscription not yet active".into()));
        }
        Ok(())
    }

    /// Whether the current period has ended at `now`.
    pub fn period_ended(&self, now: u64) -> bool {
        now > self.period_end
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.provider_sig = None;
        let v = serde_json::to_value(&clone).expect("subscription serializes");
        serde_json::to_vec(&v).expect("subscription serializes")
    }
}

/// A consent receipt: principal authorization (or policy-based
/// auto-consent) for a subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsentReceipt {
    pub consent_id: String,
    pub principal: Did,
    pub subscription_id: String,
    /// "explicit" | "policy_auto"
    pub mode: String,
    pub amount: f64,
    pub currency: String,
    pub granted_at: u64,
    #[serde(default)]
    pub sig: Option<String>,
}

impl ConsentReceipt {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig = None;
        let v = serde_json::to_value(&clone).expect("consent serializes");
        serde_json::to_vec(&v).expect("consent serializes")
    }

    pub fn sign(mut self, principal: &AgentIdentity) -> Self {
        self.sig = Some(principal.sign(&self.canonical_bytes()).to_hex());
        self
    }

    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.principal,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }
}

/// The 402 initiation response (wire format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionRequired {
    pub error: String,
    pub checkout_url: String,
    pub price_options: Vec<PriceOption>,
    pub user_consent_required: bool,
    #[serde(default)]
    pub trial_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceOption {
    pub tier: String,
    pub amount: String,
    pub currency: String,
    pub interval: String,
}

/// Parameters for starting a subscription via the manager.
#[derive(Debug, Clone)]
pub struct StartParams {
    pub subscriber: Did,
    pub capability: String,
    pub tier: String,
    pub price_hash: String,
    pub period_days: u64,
    pub renewable: bool,
    pub consent: ConsentReceipt,
}

/// A subscription manager (one provider's view).
#[derive(Debug, Default)]
pub struct SubscriptionManager {
    subscriptions: Vec<Subscription>,
    consent_log: Vec<ConsentReceipt>,
}

impl SubscriptionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluate consent: explicit consent or policy-based auto-consent
    /// within the declared budget. Returns a signed consent receipt.
    pub fn evaluate_consent(
        &self,
        principal: &AgentIdentity,
        subscription_id: &str,
        amount: f64,
        currency: &str,
        budget_remaining: f64,
        auto_consent_allowed: bool,
    ) -> Result<ConsentReceipt> {
        if amount > budget_remaining {
            return Err(Error::AutonomyViolation(format!(
                "subscription cost {amount} exceeds remaining budget {budget_remaining}"
            )));
        }
        if !auto_consent_allowed {
            return Err(Error::Unauthorized(
                "explicit consent required for this subscription".into(),
            ));
        }
        let receipt = ConsentReceipt {
            consent_id: crate::new_id("cons"),
            principal: principal.did().clone(),
            subscription_id: subscription_id.into(),
            mode: "policy_auto".into(),
            amount,
            currency: currency.into(),
            granted_at: now_unix(),
            sig: None,
        }
        .sign(principal);
        Ok(receipt)
    }

    /// Start a subscription (issue token + record consent).
    pub fn start(
        &mut self,
        provider: &AgentIdentity,
        params: StartParams,
    ) -> Result<Subscription> {
        params.consent.verify()?;
        let start = now_unix();
        let sub = Subscription::issue(
            provider,
            IssueParams {
                subscriber: params.subscriber,
                capability: params.capability,
                tier: params.tier,
                price_hash: params.price_hash,
                period_start: start,
                period_end: start + params.period_days * 86400,
                renewable: params.renewable,
                trial: false,
            },
        );
        self.consent_log.push(params.consent);
        self.subscriptions.push(sub.clone());
        Ok(sub)
    }

    /// Renew a subscription if renewable and within budget.
    pub fn renew(
        &mut self,
        subscription_id: &str,
        budget_remaining: f64,
        period_days: u64,
    ) -> Result<Subscription> {
        let idx = self
            .subscriptions
            .iter()
            .position(|s| s.subscription_id == subscription_id)
            .ok_or_else(|| Error::UnknownContract(subscription_id.into()))?;
        let sub = &self.subscriptions[idx];
        if !sub.renewable {
            return Err(Error::Other("subscription is not renewable".into()));
        }
        if sub.state != SubscriptionState::Active {
            return Err(Error::Other("subscription not active".into()));
        }
        // Budget check at the subscriber level (simplified: the caller
        // passes the tree-aggregated remaining budget).
        if budget_remaining <= 0.0 {
            return Err(Error::AutonomyViolation("insufficient budget for renewal".into()));
        }
        let start = sub.period_end;
        let end = start + period_days * 86400;
        let mut renewed = sub.clone();
        renewed.period_start = start;
        renewed.period_end = end;
        renewed.state = SubscriptionState::Active;
        self.subscriptions[idx] = renewed.clone();
        Ok(renewed)
    }

    /// Suspend a subscription (e.g. non-payment).
    pub fn suspend(&mut self, subscription_id: &str) -> Result<()> {
        let idx = self.find(subscription_id)?;
        if self.subscriptions[idx].state != SubscriptionState::Active {
            return Err(Error::Other("only active subscriptions can be suspended".into()));
        }
        self.subscriptions[idx].state = SubscriptionState::Suspended;
        Ok(())
    }

    /// Cancel a subscription (symmetric: provider or subscriber).
    pub fn cancel(&mut self, subscription_id: &str) -> Result<()> {
        let idx = self.find(subscription_id)?;
        if matches!(
            self.subscriptions[idx].state,
            SubscriptionState::Cancelled | SubscriptionState::Expired
        ) {
            return Err(Error::Other("subscription already terminated".into()));
        }
        self.subscriptions[idx].state = SubscriptionState::Cancelled;
        Ok(())
    }

    /// Look up a subscription.
    pub fn get(&self, subscription_id: &str) -> Option<&Subscription> {
        self.subscriptions
            .iter()
            .find(|s| s.subscription_id == subscription_id)
    }

    fn find(&self, subscription_id: &str) -> Result<usize> {
        self.subscriptions
            .iter()
            .position(|s| s.subscription_id == subscription_id)
            .ok_or_else(|| Error::UnknownContract(subscription_id.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consent(
        principal: &AgentIdentity,
        sub_id: &str,
        amount: f64,
    ) -> ConsentReceipt {
        ConsentReceipt {
            consent_id: crate::new_id("cons"),
            principal: principal.did().clone(),
            subscription_id: sub_id.into(),
            mode: "explicit".into(),
            amount,
            currency: "EUR".into(),
            granted_at: now_unix(),
            sig: None,
        }
        .sign(principal)
    }

    #[test]
    fn subscription_issue_verify_and_tamper() {
        let provider = AgentIdentity::generate();
        let subscriber = AgentIdentity::generate();
        let sub = Subscription::issue(
            &provider,
            IssueParams {
                subscriber: subscriber.did().clone(),
                capability: "cap:data:api".into(),
                tier: "pro".into(),
                price_hash: "sha256:price".into(),
                period_start: now_unix() - 10,
                period_end: now_unix() + 3600,
                renewable: true,
                trial: false,
            },
        );
        assert!(sub.verify(now_unix()).is_ok());
        assert!(!sub.period_ended(now_unix()));

        let mut bad = sub.clone();
        bad.tier = "enterprise".into(); // tamper
        assert!(bad.verify(now_unix()).is_err());
    }

    #[test]
    fn consent_respects_budget_and_policy() {
        let principal = AgentIdentity::generate();
        let manager = SubscriptionManager::new();
        // Within budget + auto-consent -> ok.
        let receipt = manager
            .evaluate_consent(&principal, "urn:gap:sub:x", 9.99, "EUR", 100.0, true)
            .unwrap();
        assert!(receipt.verify().is_ok());
        // Over budget -> denied.
        assert!(
            manager
                .evaluate_consent(&principal, "urn:gap:sub:x", 150.0, "EUR", 100.0, true)
                .is_err()
        );
        // Auto-consent not allowed -> denied.
        assert!(
            manager
                .evaluate_consent(&principal, "urn:gap:sub:x", 9.99, "EUR", 100.0, false)
                .is_err()
        );
    }

    #[test]
    fn full_subscription_lifecycle() {
        let provider = AgentIdentity::generate();
        let subscriber = AgentIdentity::generate();
        let mut manager = SubscriptionManager::new();

        let c = consent(&subscriber, "urn:gap:sub:1", 9.99);
        let sub = manager
            .start(
                &provider,
                StartParams {
                    subscriber: subscriber.did().clone(),
                    capability: "cap:data:api".into(),
                    tier: "pro".into(),
                    price_hash: "sha256:price".into(),
                    period_days: 30,
                    renewable: true,
                    consent: c,
                },
            )
            .unwrap();
        assert_eq!(sub.state, SubscriptionState::Active);
        assert_eq!(manager.subscriptions.len(), 1);
        assert_eq!(manager.consent_log.len(), 1);

        // Renew with budget.
        let renewed = manager.renew(&sub.subscription_id, 50.0, 30).unwrap();
        assert!(renewed.period_start > sub.period_start);
        // Renew without budget -> denied.
        assert!(manager.renew(&sub.subscription_id, 0.0, 30).is_err());

        // Suspend then cancel.
        manager.suspend(&sub.subscription_id).unwrap();
        manager.cancel(&sub.subscription_id).unwrap();
        assert_eq!(
            manager.get(&sub.subscription_id).unwrap().state,
            SubscriptionState::Cancelled
        );
        // Double cancel fails.
        assert!(manager.cancel(&sub.subscription_id).is_err());
    }

    #[test]
    fn non_renewable_subscription_rejects_renewal() {
        let provider = AgentIdentity::generate();
        let subscriber = AgentIdentity::generate();
        let mut manager = SubscriptionManager::new();
        let c = consent(&subscriber, "urn:gap:sub:2", 5.0);
        let sub = manager
            .start(
                &provider,
                StartParams {
                    subscriber: subscriber.did().clone(),
                    capability: "cap:x".into(),
                    tier: "basic".into(),
                    price_hash: "h".into(),
                    period_days: 30,
                    renewable: false, // not renewable
                    consent: c,
                },
            )
            .unwrap();
        assert!(manager.renew(&sub.subscription_id, 100.0, 30).is_err());
    }

    #[test]
    fn unknown_subscription_errors() {
        let mut manager = SubscriptionManager::new();
        assert!(manager.cancel("urn:gap:sub:nope").is_err());
        assert!(manager.get("urn:gap:sub:nope").is_none());
    }
}
