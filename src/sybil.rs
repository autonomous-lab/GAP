//! Sybil resistance (RFC-0007).
//!
//! Rate limits, budgets, reputation weighting, and rewards MUST
//! aggregate per delegation tree, so a single principal cannot spawn
//! hundreds of sub-agents to inflate reputation, flood negotiations,
//! or farm rewards.

use crate::delegation::{TokenChain, MAX_DEPTH};
use crate::error::{Error, Result};
use crate::identity::Did;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Actions a sub-agent MUST NOT perform regardless of mandate scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestrictedAction {
    RepRecord,
    MarketplaceReview,
    GovPollVote,
    /// Contract acceptance above the human-required threshold.
    CtrAcceptAboveThreshold,
    /// Granting restricted actions onward.
    DlgGrantRestricted,
}

impl RestrictedAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            RestrictedAction::RepRecord => "rep.record",
            RestrictedAction::MarketplaceReview => "marketplace.review",
            RestrictedAction::GovPollVote => "gov.poll.vote",
            RestrictedAction::CtrAcceptAboveThreshold => "ctr.accept.above_threshold",
            RestrictedAction::DlgGrantRestricted => "dlg.grant.restricted",
        }
    }
}

/// Resolves the root DID of a delegation chain.
pub fn tree_root(chain: &TokenChain) -> Did {
    chain
        .root()
        .cloned()
        .unwrap_or_else(|| chain.delegate().cloned().expect("empty chain"))
}

/// Whether an actor is a sub-agent (any delegate in a chain whose
/// delegator is not the root itself, i.e. depth > 1).
pub fn is_sub_agent(chain: &TokenChain) -> bool {
    chain.tokens.len() > 1
}

/// Enforce the restricted-action list.
pub fn enforce_restricted(chain: &TokenChain, action: RestrictedAction) -> Result<()> {
    if is_sub_agent(chain) {
        return Err(Error::AutonomyViolation(format!(
            "restricted action {} forbidden for sub-agent",
            action.as_str()
        )));
    }
    Ok(())
}

/// Per-tree rate counters.
#[derive(Debug, Clone, Default)]
pub struct RateCounters {
    /// per-minute invocation count and window start.
    invocations_minute: (u64, u32),
    /// per-day contract count.
    contracts_day: (u64, u32),
}

impl RateCounters {
    /// Record an invocation; returns Err if over the per-minute cap.
    pub fn record_invocation(&mut self, now: u64, cap_per_minute: u32) -> Result<()> {
        let minute = now / 60;
        if self.invocations_minute.0 != minute {
            self.invocations_minute = (minute, 0);
        }
        if self.invocations_minute.1 >= cap_per_minute {
            return Err(Error::AutonomyViolation(format!(
                "per-minute rate limit exceeded for tree ({} > {})",
                self.invocations_minute.1, cap_per_minute
            )));
        }
        self.invocations_minute.1 += 1;
        Ok(())
    }

    /// Record a contract; returns Err if over the per-day cap.
    pub fn record_contract(&mut self, now: u64, cap_per_day: u32) -> Result<()> {
        let day = now / 86400;
        if self.contracts_day.0 != day {
            self.contracts_day = (day, 0);
        }
        if self.contracts_day.1 >= cap_per_day {
            return Err(Error::AutonomyViolation(format!(
                "per-day contract limit exceeded for tree ({} > {})",
                self.contracts_day.1, cap_per_day
            )));
        }
        self.contracts_day.1 += 1;
        Ok(())
    }
}

/// Aggregates limits per delegation tree.
#[derive(Debug, Default)]
pub struct TreeBucket {
    /// root DID -> (counters)
    buckets: HashMap<Did, RateCounters>,
}

impl TreeBucket {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enforce and record an invocation for the tree.
    pub fn record_invocation(
        &mut self,
        chain: &TokenChain,
        now: u64,
        cap_per_minute: u32,
    ) -> Result<()> {
        let root = tree_root(chain);
        self.buckets
            .entry(root)
            .or_default()
            .record_invocation(now, cap_per_minute)
    }

    /// Enforce and record a contract for the tree.
    pub fn record_contract(
        &mut self,
        chain: &TokenChain,
        now: u64,
        cap_per_day: u32,
    ) -> Result<()> {
        let root = tree_root(chain);
        self.buckets
            .entry(root)
            .or_default()
            .record_contract(now, cap_per_day)
    }
}

/// A coordinated behavior score in [0, 1]: an estimate of the
/// probability that a set of agents act in concert under one root.
///
/// v0.1 heuristic: based on shared endpoint patterns and synchronized
/// timing. Higher = more likely coordinated (weighted as one voice).
#[derive(Debug, Clone, Copy)]
pub struct CoordinatedScore(f64);

impl CoordinatedScore {
    /// Compute from a list of (endpoint, timestamp) samples.
    pub fn compute(samples: &[(String, u64)]) -> Self {
        if samples.len() < 2 {
            return Self(0.0);
        }
        // Same endpoint share.
        let mut endpoints: HashMap<&str, usize> = HashMap::new();
        for (ep, _) in samples {
            *endpoints.entry(ep).or_insert(0) += 1;
        }
        let max_share = endpoints.values().max().copied().unwrap_or(0) as f64
            / samples.len() as f64;

        // Timing regularity: fraction of gaps within 5s of each other.
        let mut gaps: Vec<u64> = samples.windows(2).map(|w| w[1].1 - w[0].1).collect();
        gaps.sort_unstable();
        let median_gap = gaps[gaps.len() / 2].max(1);
        let tight = gaps
            .iter()
            .filter(|g| g.abs_diff(median_gap) <= 5)
            .count() as f64
            / gaps.len() as f64;

        let score = 0.6 * max_share + 0.4 * tight;
        Self(score.clamp(0.0, 1.0))
    }

    pub fn score(&self) -> f64 {
        self.0
    }

    /// Whether this should be treated as one voice (score ≥ 0.7).
    pub fn treated_as_one_voice(&self) -> bool {
        self.0 >= 0.7
    }
}

/// One negotiation bid per tree per round.
#[derive(Debug, Default)]
pub struct NegotiationGuard {
    /// (root, round) -> has bid
    bids: HashMap<(Did, String), bool>,
}

impl NegotiationGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a bid; rejects a second bid from the same tree in the
    /// same round.
    pub fn register_bid(&mut self, chain: &TokenChain, round_id: &str) -> Result<()> {
        let root = tree_root(chain);
        let key = (root.clone(), round_id.to_string());
        if self.bids.get(&key).copied().unwrap_or(false) {
            return Err(Error::AutonomyViolation(format!(
                "tree {root} already bid in round {round_id}"
            )));
        }
        self.bids.insert(key, true);
        Ok(())
    }
}

/// Verify that MAX_DEPTH is imported from delegation (kept for clarity
/// of the module's contract with RFC-0001).
#[allow(dead_code)]
const _: usize = MAX_DEPTH;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::{Budget, DelegationToken, Mandate};
    use crate::identity::AgentIdentity;

    fn mandate() -> Mandate {
        Mandate {
            capabilities: vec!["cap:a".into()],
            budget: Budget::default(),
            autonomy_level: "propose".into(),
            jurisdictions: vec![],
            channels: vec![],
            expires_at: crate::message::now_unix() + 3600,
            mode: "standing".into(),
        }
    }

    fn chain_of_depth(depth: usize) -> TokenChain {
        let mut chain = TokenChain::default();
        let mut prev = AgentIdentity::generate();
        let root = prev.did().clone();
        for i in 0..depth {
            let next = AgentIdentity::generate();
            let t = DelegationToken::issue(
                &prev,
                next.did().clone(),
                root.clone(),
                if i == 0 {
                    "urn:gap:dlg:0".into()
                } else {
                    format!("urn:gap:dlg:p{i}")
                },
                mandate(),
            );
            chain.push(t).unwrap();
            prev = next;
        }
        chain
    }

    #[test]
    fn tree_root_resolves() {
        let chain = chain_of_depth(3);
        let root = tree_root(&chain);
        assert_eq!(root, *chain.root().unwrap());
    }

    #[test]
    fn sub_agent_restricted_actions() {
        let single = chain_of_depth(1);
        assert!(!is_sub_agent(&single));
        assert!(enforce_restricted(&single, RestrictedAction::RepRecord).is_ok());

        let multi = chain_of_depth(3);
        assert!(is_sub_agent(&multi));
        for action in [
            RestrictedAction::RepRecord,
            RestrictedAction::MarketplaceReview,
            RestrictedAction::GovPollVote,
            RestrictedAction::CtrAcceptAboveThreshold,
        ] {
            assert!(enforce_restricted(&multi, action).is_err());
        }
    }

    #[test]
    fn tree_bucket_rate_limits_per_tree() {
        let mut bucket = TreeBucket::new();
        let now = 1_000_000u64;
        let chain = chain_of_depth(2);
        // 2 invocations under cap 3.
        bucket.record_invocation(&chain, now, 3).unwrap();
        bucket.record_invocation(&chain, now, 3).unwrap();
        bucket.record_invocation(&chain, now, 3).unwrap();
        // 4th exceeds.
        assert!(bucket.record_invocation(&chain, now, 3).is_err());

        // A different tree is not affected.
        let other = chain_of_depth(2);
        bucket.record_invocation(&other, now, 3).unwrap();

        // Contract caps.
        let mut bucket2 = TreeBucket::new();
        bucket2.record_contract(&chain, now, 2).unwrap();
        bucket2.record_contract(&chain, now, 2).unwrap();
        assert!(bucket2.record_contract(&chain, now, 2).is_err());
    }

    #[test]
    fn negotiation_one_bid_per_tree() {
        let mut guard = NegotiationGuard::new();
        let chain = chain_of_depth(2);
        guard.register_bid(&chain, "round-1").unwrap();
        assert!(guard.register_bid(&chain, "round-1").is_err());
        // New round allows a bid.
        guard.register_bid(&chain, "round-2").unwrap();
        // Different tree allowed in round 1.
        let other = chain_of_depth(1);
        guard.register_bid(&other, "round-1").unwrap();
    }

    #[test]
    fn coordinated_score_heuristics() {
        // Identical endpoints, tight timing -> one voice.
        let samples = vec![
            ("https://a.example".to_string(), 1000),
            ("https://a.example".to_string(), 1002),
            ("https://a.example".to_string(), 1004),
        ];
        let score = CoordinatedScore::compute(&samples);
        assert!(score.treated_as_one_voice());

        // Diverse endpoints, loose timing -> independent.
        let samples2 = vec![
            ("https://a.example".to_string(), 1000),
            ("https://b.example".to_string(), 2000),
            ("https://c.example".to_string(), 3500),
        ];
        let score2 = CoordinatedScore::compute(&samples2);
        assert!(!score2.treated_as_one_voice());
        assert!(score2.score() < score.score());

        // Fewer than 2 samples: 0.
        let score3 = CoordinatedScore::compute(&[("https://a.example".to_string(), 1000)]);
        assert_eq!(score3.score(), 0.0);
    }
}
