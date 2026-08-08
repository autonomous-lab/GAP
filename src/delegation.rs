//! Delegation layer (RFC-0001).
//!
//! Delegation Tokens allow an agent to act on behalf of another within
//! a bounded mandate. This is the primitive that unlocks multi-agent
//! coordination: sub-agents, budgeted authority, and workflow
//! composition (RFC-0002).

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};

/// A bounded mandate granted by a delegator to a delegate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mandate {
    /// Capability ids the delegate may contract for.
    pub capabilities: Vec<String>,
    /// Budget constraints (per contract, per day).
    pub budget: Budget,
    /// Autonomy level ceiling (part 06): "propose" | "execute-notify" | "execute-certified".
    pub autonomy_level: String,
    #[serde(default)]
    pub jurisdictions: Vec<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    pub expires_at: u64,
    /// "standing" (repeatable) or "one_shot" (auto-expires after first use).
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "standing".into()
}

/// Budget constraints on a mandate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default)]
    pub per_contract: Option<f64>,
    #[serde(default)]
    pub per_day: Option<f64>,
    #[serde(default)]
    pub currency: String,
}

/// A signed delegation token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationToken {
    pub delegation_id: String,
    pub delegator: Did,
    pub delegate: Did,
    /// The root principal of the delegation tree (self for the root).
    pub root: Did,
    /// Parent token id; `"urn:gap:dlg:0"` for the root.
    pub parent: String,
    pub mandate: Mandate,
    #[serde(default)]
    pub used: bool,
    pub issued_at: u64,
    #[serde(default)]
    pub delegator_sig: Option<String>,
}

impl DelegationToken {
    /// Issue and sign a new mandate.
    pub fn issue(
        delegator: &AgentIdentity,
        delegate: Did,
        root: Did,
        parent: String,
        mandate: Mandate,
    ) -> Self {
        let mut t = Self {
            delegation_id: crate::new_id("dlg"),
            delegator: delegator.did().clone(),
            delegate,
            root,
            parent,
            mandate,
            used: false,
            issued_at: crate::message::now_unix(),
            delegator_sig: None,
        };
        t.resign(delegator);
        t
    }

    /// Re-sign after mutation.
    pub fn resign(&mut self, delegator: &AgentIdentity) {
        self.delegator_sig = None;
        let canonical = self.canonical_bytes();
        self.delegator_sig = Some(delegator.sign(&canonical).to_hex());
    }

    /// Verify the delegator's signature.
    pub fn verify(&self) -> Result<()> {
        let sig_hex = self
            .delegator_sig
            .as_ref()
            .ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.delegator,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }

    /// Whether the mandate has expired at `now`.
    pub fn is_expired(&self, now: u64) -> bool {
        now > self.mandate.expires_at
    }

    /// Whether this token has been spent (one-shot mode).
    pub fn is_spent(&self) -> bool {
        self.used
    }

    /// Mark as used (one-shot mode).
    pub fn mark_used(&mut self) {
        self.used = true;
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.delegator_sig = None;
        let v = serde_json::to_value(&clone).expect("token serializes");
        serde_json::to_vec(&v).expect("token serializes")
    }
}

/// The full chain from root to a delegate.
#[derive(Debug, Clone, Default)]
pub struct TokenChain {
    pub tokens: Vec<DelegationToken>,
}

/// Maximum delegation depth (matches Geta.Team's 5-hop limit).
pub const MAX_DEPTH: usize = 5;

impl TokenChain {
    /// The delegate (last token's delegate, or the only token's delegator
    /// when the chain is the root itself).
    pub fn delegate(&self) -> Option<&Did> {
        self.tokens.last().map(|t| &t.delegate)
    }

    /// The root DID of the tree.
    pub fn root(&self) -> Option<&Did> {
        self.tokens.first().map(|t| &t.root)
    }

    /// Push a token onto the chain, enforcing the no-escalation rule
    /// against the current top.
    pub fn push(&mut self, token: DelegationToken) -> Result<()> {
        if self.tokens.len() >= MAX_DEPTH {
            return Err(Error::Other(format!(
                "delegation depth exceeds maximum of {MAX_DEPTH}"
            )));
        }
        if let Some(parent) = self.tokens.last() {
            // The new token's delegator must be the current delegate.
            if token.delegator != parent.delegate {
                return Err(Error::Unauthorized(
                    "token delegator is not the current delegate".into(),
                ));
            }
            enforce_no_escalation(parent, &token)?;
        }
        self.tokens.push(token);
        Ok(())
    }

    /// Verify the entire chain: signatures, parent linkage, expiry,
    /// and root consistency.
    pub fn verify(&self, now: u64) -> Result<()> {
        let mut expected_parent = "urn:gap:dlg:0".to_string();
        let mut expected_delegator: Option<Did> = None;
        let root = self.tokens.first().map(|t| t.root.clone());

        for (i, token) in self.tokens.iter().enumerate() {
            token.verify()?;
            if token.is_expired(now) {
                return Err(Error::Other("delegation token expired".into()));
            }
            if token.parent != expected_parent {
                return Err(Error::Other("delegation chain parent mismatch".into()));
            }
            if i == 0 {
                // Root token: delegator == root == delegate is allowed
                // (self-delegation), but the root field must be self.
                if token.root != token.delegator {
                    return Err(Error::Other("root token has foreign root".into()));
                }
            } else if let Some(prev_delegate) = expected_delegator.as_ref() {
                if &token.delegator != prev_delegate {
                    return Err(Error::Unauthorized(
                        "chain hop is not signed by the previous delegate".into(),
                    ));
                }
                if token.root != *root.as_ref().unwrap() {
                    return Err(Error::Other("delegation tree root changed".into()));
                }
            }
            expected_parent = token.delegation_id.clone();
            expected_delegator = Some(token.delegate.clone());
        }
        Ok(())
    }

    /// Total budget available at the root level for a given day, capped
    /// by every mandate in the chain (budgets shrink toward the leaf).
    pub fn effective_budget(&self) -> Budget {
        let mut per_day: Option<f64> = None;
        let mut per_contract: Option<f64> = None;
        let mut currency = String::new();
        for token in &self.tokens {
            if let Some(d) = token.mandate.budget.per_day {
                per_day = Some(per_day.map_or(d, |cur| cur.min(d)));
            }
            if let Some(c) = token.mandate.budget.per_contract {
                per_contract = Some(per_contract.map_or(c, |cur| cur.min(c)));
            }
            if currency.is_empty() {
                currency = token.mandate.budget.currency.clone();
            }
        }
        Budget {
            per_contract,
            per_day,
            currency,
        }
    }

    /// Whether the delegate may contract for `capability` (checked
    /// against every mandate in the chain).
    pub fn allows_capability(&self, capability: &str) -> bool {
        self.tokens
            .iter()
            .all(|t| t.mandate.capabilities.iter().any(|c| c == capability))
    }

    /// Whether a token is one-shot and already spent.
    pub fn any_spent(&self) -> bool {
        self.tokens.iter().any(|t| t.is_spent())
    }
}

/// The no-escalation rule: a child mandate must be no broader than its
/// parent.
pub fn enforce_no_escalation(parent: &DelegationToken, child: &DelegationToken) -> Result<()> {
    // Capabilities: child ⊆ parent.
    for cap in &child.mandate.capabilities {
        if !parent.mandate.capabilities.contains(cap) {
            return Err(Error::AutonomyViolation(format!(
                "child mandate grants capability {cap} not held by parent"
            )));
        }
    }
    // Budget: child ≤ parent.
    if let Some(p) = parent.mandate.budget.per_contract {
        if let Some(c) = child.mandate.budget.per_contract {
            if c > p {
                return Err(Error::AutonomyViolation(
                    "child per-contract budget exceeds parent".into(),
                ));
            }
        }
    }
    if let Some(p) = parent.mandate.budget.per_day {
        if let Some(c) = child.mandate.budget.per_day {
            if c > p {
                return Err(Error::AutonomyViolation(
                    "child per-day budget exceeds parent".into(),
                ));
            }
        }
    }
    // Autonomy: child ≤ parent.
    let rank = |l: &str| -> u8 {
        match l {
            "propose" => 0,
            "execute-notify" => 1,
            "execute-certified" => 2,
            _ => 0,
        }
    };
    if rank(&child.mandate.autonomy_level) > rank(&parent.mandate.autonomy_level) {
        return Err(Error::AutonomyViolation(
            "child autonomy level exceeds parent".into(),
        ));
    }
    // Expiry: child ≤ parent.
    if child.mandate.expires_at > parent.mandate.expires_at {
        return Err(Error::AutonomyViolation(
            "child mandate outlives parent".into(),
        ));
    }
    Ok(())
}

/// Tracks per-tree daily spend for budget enforcement.
#[derive(Debug, Default)]
pub struct BudgetTracker {
    /// root DID -> (day_key, spent)
    spent: std::collections::HashMap<Did, (u64, f64)>,
}

impl BudgetTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempt to spend `amount` against the root's daily budget.
    /// Returns Ok(()) and records the spend if within budget.
    pub fn try_spend(&mut self, root: &Did, amount: f64, per_day_limit: f64) -> Result<()> {
        let day = crate::message::now_unix() / 86400;
        let entry = self.spent.entry(root.clone()).or_insert((day, 0.0));
        if entry.0 != day {
            *entry = (day, 0.0);
        }
        if entry.1 + amount > per_day_limit {
            return Err(Error::AutonomyViolation(format!(
                "daily budget exceeded for tree {root}: {:.2} + {:.2} > {:.2}",
                entry.1, amount, per_day_limit
            )));
        }
        entry.1 += amount;
        Ok(())
    }

    /// Current spend for a tree on the current day.
    pub fn spent_today(&self, root: &Did) -> f64 {
        let day = crate::message::now_unix() / 86400;
        self.spent
            .get(root)
            .map_or(0.0, |(d, s)| if *d == day { *s } else { 0.0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mandate(per_day: f64, autonomy: &str, expires_in: u64) -> Mandate {
        Mandate {
            capabilities: vec!["cap:a".into(), "cap:b".into()],
            budget: Budget {
                per_contract: Some(10.0),
                per_day: Some(per_day),
                currency: "EUR".into(),
            },
            autonomy_level: autonomy.into(),
            jurisdictions: vec!["EU".into()],
            channels: vec![],
            expires_at: crate::message::now_unix() + expires_in,
            mode: "standing".into(),
        }
    }

    #[test]
    fn chain_verifies_and_detects_tampering() {
        let root = AgentIdentity::generate();
        let mid = AgentIdentity::generate();
        let leaf = AgentIdentity::generate();

        let t1 = DelegationToken::issue(
            &root,
            mid.did().clone(),
            root.did().clone(),
            "urn:gap:dlg:0".into(),
            mandate(200.0, "execute-notify", 3600),
        );
        let t2 = DelegationToken::issue(
            &mid,
            leaf.did().clone(),
            root.did().clone(),
            t1.delegation_id.clone(),
            mandate(100.0, "execute-notify", 1800),
        );

        let mut chain = TokenChain::default();
        chain.push(t1).unwrap();
        chain.push(t2).unwrap();
        assert!(chain.verify(crate::message::now_unix()).is_ok());
        assert_eq!(chain.root().unwrap(), root.did());
        assert_eq!(chain.delegate().unwrap(), leaf.did());

        // Tamper: change the leaf's delegate.
        let mut chain2 = TokenChain::default();
        let mut t1b = DelegationToken::issue(
            &root,
            mid.did().clone(),
            root.did().clone(),
            "urn:gap:dlg:0".into(),
            mandate(200.0, "execute-notify", 3600),
        );
        t1b.delegate = leaf.did().clone(); // forged after signing
        chain2.push(t1b).unwrap();
        assert!(chain2.verify(crate::message::now_unix()).is_err());
    }

    #[test]
    fn escalation_is_rejected() {
        let root = AgentIdentity::generate();
        let mid = AgentIdentity::generate();
        let leaf = AgentIdentity::generate();

        let t1 = DelegationToken::issue(
            &root,
            mid.did().clone(),
            root.did().clone(),
            "urn:gap:dlg:0".into(),
            mandate(100.0, "execute-notify", 3600),
        );
        // Child tries to escalate: bigger budget, higher autonomy,
        // longer expiry.
        let bad = DelegationToken::issue(
            &mid,
            leaf.did().clone(),
            root.did().clone(),
            t1.delegation_id.clone(),
            Mandate {
                capabilities: vec!["cap:a".into(), "cap:b".into()],
                budget: Budget {
                    per_contract: Some(10.0),
                    per_day: Some(500.0),
                    currency: "EUR".into(),
                },
                autonomy_level: "execute-certified".into(),
                jurisdictions: vec!["EU".into()],
                channels: vec![],
                expires_at: crate::message::now_unix() + 7200,
                mode: "standing".into(),
            },
        );
        let mut chain = TokenChain::default();
        chain.push(t1).unwrap();
        assert!(chain.push(bad).is_err());
    }

    #[test]
    fn chain_respects_max_depth() {
        let mut chain = TokenChain::default();
        let mut prev = AgentIdentity::generate();
        let root_did = prev.did().clone();
        for i in 0..MAX_DEPTH + 1 {
            let next = AgentIdentity::generate();
            let t = DelegationToken::issue(
                &prev,
                next.did().clone(),
                root_did.clone(),
                if i == 0 {
                    "urn:gap:dlg:0".into()
                } else {
                    format!("urn:gap:dlg:parent-{i}")
                },
                mandate(1000.0, "execute-notify", 3600),
            );
            let res = chain.push(t);
            if i == MAX_DEPTH {
                assert!(res.is_err());
            } else {
                res.unwrap();
            }
            prev = next;
        }
    }

    #[test]
    fn budget_aggregates_to_the_most_restrictive() {
        let root = AgentIdentity::generate();
        let mid = AgentIdentity::generate();
        let leaf = AgentIdentity::generate();

        let t1 = DelegationToken::issue(
            &root,
            mid.did().clone(),
            root.did().clone(),
            "urn:gap:dlg:0".into(),
            mandate(200.0, "execute-notify", 3600),
        );
        let t2 = DelegationToken::issue(
            &mid,
            leaf.did().clone(),
            root.did().clone(),
            t1.delegation_id.clone(),
            mandate(50.0, "execute-notify", 3600),
        );
        let mut chain = TokenChain::default();
        chain.push(t1).unwrap();
        chain.push(t2).unwrap();
        assert_eq!(chain.effective_budget().per_day, Some(50.0));

        // Capability whitelist: all mandates must allow it.
        assert!(chain.allows_capability("cap:a"));
        assert!(!chain.allows_capability("cap:c"));
    }

    #[test]
    fn budget_tracker_enforces_daily_limit() {
        let root = AgentIdentity::generate();
        let mut tracker = BudgetTracker::new();
        tracker.try_spend(root.did(), 80.0, 100.0).unwrap();
        assert_eq!(tracker.spent_today(root.did()), 80.0);
        assert!(tracker.try_spend(root.did(), 30.0, 100.0).is_err());
        assert_eq!(tracker.spent_today(root.did()), 80.0);
    }

    #[test]
    fn one_shot_token_spends() {
        let root = AgentIdentity::generate();
        let delegate = AgentIdentity::generate();
        let mut t = DelegationToken::issue(
            &root,
            delegate.did().clone(),
            root.did().clone(),
            "urn:gap:dlg:0".into(),
            mandate(10.0, "propose", 3600),
        );
        assert!(!t.is_spent());
        t.mark_used();
        assert!(t.is_spent());
    }

    #[test]
    fn expired_token_fails_verification() {
        let root = AgentIdentity::generate();
        let delegate = AgentIdentity::generate();
        let t = DelegationToken::issue(
            &root,
            delegate.did().clone(),
            root.did().clone(),
            "urn:gap:dlg:0".into(),
            mandate(10.0, "propose", 1), // expires in 1s
        );
        let mut chain = TokenChain::default();
        chain.push(t).unwrap();
        // Simulate the future.
        assert!(chain.verify(crate::message::now_unix() + 100).is_err());
    }
}
