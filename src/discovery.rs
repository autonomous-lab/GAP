//! Discovery layer (GAP spec part 02).
//!
//! Agents announce what they can do; clients query registries to find
//! them. Registries verify signatures, enforce TTLs, and return signed
//! results.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A machine-readable description of something an agent can do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Free-form JSON schema for the input.
    #[serde(default)]
    pub input: serde_json::Value,
    /// Free-form JSON schema for the output.
    #[serde(default)]
    pub output: serde_json::Value,
    #[serde(default)]
    pub price: Option<Price>,
    /// Autonomy levels this capability can operate at.
    #[serde(default)]
    pub autonomy: Vec<String>,
}

/// A price in GAP's monetary model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Price {
    #[serde(deserialize_with = "crate::contract::de_f64_from_string")]
    pub amount: f64,
    pub currency: String,
    /// One of: fixed, per_unit, subscription, commission.
    pub model: String,
}

/// A signed capability announcement (what an agent offers + how to reach it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Announcement {
    pub agent_did: Did,
    pub capabilities: Vec<Capability>,
    pub reachability: Vec<Reachability>,
    #[serde(default)]
    pub autonomy_levels: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub regions: Vec<String>,
    pub ttl_seconds: u64,
    /// Signature by the announcing agent over the canonical payload.
    #[serde(default)]
    pub signature: Option<String>,
}

/// How to reach an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reachability {
    pub transport: String,
    pub endpoint: String,
}

impl Announcement {
    /// Create and sign an announcement.
    pub fn signed(
        agent: &AgentIdentity,
        capabilities: Vec<Capability>,
        reachability: Vec<Reachability>,
        ttl_seconds: u64,
    ) -> Self {
        let mut ann = Self {
            agent_did: agent.did().clone(),
            capabilities,
            reachability,
            autonomy_levels: vec!["propose".into(), "execute-notify".into()],
            languages: vec![],
            regions: vec![],
            ttl_seconds,
            signature: None,
        };
        ann.resign(agent);
        ann
    }

    /// Re-sign after mutating fields. Any mutation invalidates the old
    /// signature, so call this before announcing a modified copy.
    pub fn resign(&mut self, agent: &AgentIdentity) {
        self.signature = None;
        let canonical = serde_json::to_vec(self).expect("announcement serializes");
        self.signature = Some(agent.sign(&canonical).to_hex());
    }

    /// Verify the announcement signature.
    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.signature.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        let mut clone = self.clone();
        clone.signature = None;
        let canonical = serde_json::to_vec(&clone).expect("announcement serializes");
        crate::identity::verify_signature(
            &self.agent_did,
            &canonical,
            &crate::identity::Signature(sig_bytes),
        )
    }
}

/// A discovery query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    /// Required capability name (exact match).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub regions: Vec<String>,
    #[serde(default)]
    pub max_price: Option<f64>,
    #[serde(default)]
    pub required_autonomy: Option<String>,
    #[serde(default)]
    pub min_reputation: Option<f64>,
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            name: None,
            languages: vec![],
            regions: vec![],
            max_price: None,
            required_autonomy: None,
            min_reputation: None,
            max_results: default_max_results(),
        }
    }
}

fn default_max_results() -> usize {
    20
}

/// A minimal in-memory registry, sufficient to validate the protocol
/// mechanics. A production registry persists announcements and signs
/// query results.
#[derive(Debug, Default)]
pub struct Registry {
    announcements: HashMap<Did, (Announcement, u64)>,
    /// Optional reputation snapshot per agent (fed by attestations).
    reputations: HashMap<Did, f64>,
    /// Deregistered agents and when (spec 02 §2.5): a query can then
    /// distinguish "gone" from "never existed".
    tombstones: HashMap<Did, u64>,
    /// Optional test clock override; when set, replaces `now_unix()`.
    #[cfg(test)]
    now_override: Option<u64>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the registry's notion of "now" for deterministic TTL
    /// testing (tests only).
    #[cfg(test)]
    pub fn set_now(&mut self, now: u64) {
        self.now_override = Some(now);
    }

    #[cfg(test)]
    fn now(&self) -> u64 {
        self.now_override.unwrap_or_else(crate::message::now_unix)
    }

    #[cfg(not(test))]
    fn now(&self) -> u64 {
        crate::message::now_unix()
    }

    /// Store a verified announcement. Returns an error if the signature
    /// is invalid (registry MUST reject unsigned/forged announcements).
    pub fn announce(&mut self, ann: Announcement) -> Result<()> {
        ann.verify()?;
        let ttl = ann.ttl_seconds;
        let expires = self.now().saturating_add(ttl);
        self.announcements
            .insert(ann.agent_did.clone(), (ann, expires));
        Ok(())
    }

    /// Record a reputation snapshot for an agent (e.g. from attestations
    /// published after executions).
    /// `cap.deregister` — withdraw an announcement and leave a
    /// tombstone (spec 02 §2.5), so a query can tell "gone" from
    /// "never existed". Re-announcing clears it.
    pub fn deregister(&mut self, agent: &Did) -> bool {
        let existed = self.announcements.remove(agent).is_some();
        self.tombstones.insert(agent.clone(), self.now());
        existed
    }

    /// When this agent deregistered, if it did.
    pub fn tombstone(&self, agent: &Did) -> Option<u64> {
        self.tombstones.get(agent).copied()
    }

    pub fn set_reputation(&mut self, agent: Did, score: f64) {
        self.reputations.insert(agent, score);
    }

    /// Expire stale announcements.
    pub fn reap(&mut self) {
        let now = self.now();
        self.announcements.retain(|_, (_, expires)| *expires > now);
    }

    /// Run a query against the registry.
    ///
    /// Filtering is exact. `max_price` is evaluated against the matching
    /// capability only (not against unrelated, expensive capabilities).
    /// `min_reputation` filters on the registry's reputation snapshot.
    pub fn query(&self, q: &Query) -> Vec<Announcement> {
        let now = self.now();
        let mut hits: Vec<Announcement> = self
            .announcements
            .values()
            .filter(|(ann, expires)| {
                if *expires <= now {
                    return false;
                }
                // Name filter: at least one capability matches the name.
                let name_ok = q
                    .name
                    .as_ref()
                    .is_none_or(|n| ann.capabilities.iter().any(|c| c.name == *n));
                if !name_ok {
                    return false;
                }
                // Price filter: the matching capability (or the cheapest)
                // must be within budget.
                if let Some(max) = q.max_price {
                    let candidates: Vec<&Capability> = ann
                        .capabilities
                        .iter()
                        .filter(|c| q.name.as_ref().is_none_or(|n| c.name == *n))
                        .filter_map(|c| c.price.as_ref().map(|_| c))
                        .collect();
                    let within = candidates
                        .iter()
                        .any(|c| c.price.as_ref().is_some_and(|p| p.amount <= max));
                    // An announcement with no priced capability fails a
                    // price query only if the name matches that capability.
                    let no_priced_matching = candidates.is_empty()
                        && q.name
                            .as_ref()
                            .is_some_and(|n| ann.capabilities.iter().any(|c| c.name == *n));
                    if !within && !no_priced_matching {
                        return false;
                    }
                }
                q.languages.iter().all(|l| ann.languages.contains(l))
                    && q.regions.iter().all(|r| ann.regions.contains(r))
                    && q.required_autonomy
                        .as_ref()
                        .is_none_or(|a| ann.autonomy_levels.contains(a))
            })
            .filter(|(ann, _)| {
                q.min_reputation.is_none_or(|min| {
                    self.reputations.get(&ann.agent_did).copied().unwrap_or(0.0) >= min
                })
            })
            .map(|(ann, _)| ann.clone())
            .collect();
        hits.truncate(q.max_results);
        hits
    }

    /// Number of live announcements.
    pub fn len(&self) -> usize {
        self.announcements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.announcements.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_agent(name: &str) -> (AgentIdentity, Capability) {
        let agent = AgentIdentity::generate();
        let cap = Capability {
            id: format!("cap:{name}:lead-gen"),
            name: "lead-generation".into(),
            description: "qualify leads".into(),
            input: serde_json::json!({}),
            output: serde_json::json!({}),
            price: Some(Price {
                amount: 0.05,
                currency: "EUR".into(),
                model: "per_unit".into(),
            }),
            autonomy: vec!["propose".into(), "execute-notify".into()],
        };
        (agent, cap)
    }

    fn announce(
        reg: &mut Registry,
        agent: &AgentIdentity,
        cap: Capability,
        ttl: u64,
    ) -> Announcement {
        let ann = Announcement::signed(
            agent,
            vec![cap],
            vec![Reachability {
                transport: "https".into(),
                endpoint: format!("https://{}/gap", agent.did()),
            }],
            ttl,
        );
        reg.announce(ann.clone()).unwrap();
        ann
    }

    #[test]
    fn registry_filters_by_capability() {
        let mut reg = Registry::new();
        let (a, cap_a) = make_agent("a");
        let (b, cap_b) = make_agent("b");
        announce(&mut reg, &a, cap_a, 3600);
        announce(&mut reg, &b, cap_b, 3600);

        let q = Query {
            name: Some("lead-generation".into()),
            ..Default::default()
        };
        assert_eq!(reg.query(&q).len(), 2);
    }

    #[test]
    fn registry_rejects_forged_announcement() {
        let mut reg = Registry::new();
        let (agent, cap) = make_agent("x");
        let mut ann = Announcement::signed(&agent, vec![cap], vec![], 3600);
        // Forge: claim to be a different agent
        ann.agent_did = AgentIdentity::generate().did().clone();
        assert!(reg.announce(ann).is_err());
    }

    #[test]
    fn registry_expires_announcements_after_ttl() {
        let mut reg = Registry::new();
        reg.set_now(1_000_000);
        let (a, cap_a) = make_agent("a");
        let (b, cap_b) = make_agent("b");
        announce(&mut reg, &a, cap_a, 60); // short TTL
        announce(&mut reg, &b, cap_b, 3600); // long TTL
        assert_eq!(reg.len(), 2);

        // After 120s, agent a's announcement has expired.
        reg.set_now(1_000_120);
        reg.reap();
        assert_eq!(reg.len(), 1);
        let q = Query::default();
        assert_eq!(reg.query(&q).len(), 1);
    }

    #[test]
    fn registry_filters_by_language_region_and_autonomy() {
        let mut reg = Registry::new();
        let (a, mut cap_a) = make_agent("a");
        cap_a.name = "translation".into();
        let mut ann = Announcement::signed(&a, vec![cap_a], vec![], 3600);
        ann.languages = vec!["fr".into(), "en".into()];
        ann.regions = vec!["EU".into()];
        ann.autonomy_levels = vec!["propose".into(), "execute-notify".into()];
        ann.resign(&a); // re-sign after mutation
        reg.announce(ann).unwrap();

        // Matching language, region, autonomy
        let q = Query {
            name: Some("translation".into()),
            languages: vec!["fr".into()],
            regions: vec!["EU".into()],
            required_autonomy: Some("execute-notify".into()),
            ..Default::default()
        };
        assert_eq!(reg.query(&q).len(), 1);

        // Wrong language
        let q2 = Query {
            languages: vec!["de".into()],
            ..Default::default()
        };
        assert_eq!(reg.query(&q2).len(), 0);

        // Unsupported autonomy level
        let q3 = Query {
            required_autonomy: Some("execute-certified".into()),
            ..Default::default()
        };
        assert_eq!(reg.query(&q3).len(), 0);
    }

    #[test]
    fn registry_price_filter_applies_to_matching_capability_only() {
        let mut reg = Registry::new();
        let (agent, _) = make_agent("multi");
        let mut combined = Announcement::signed(&agent, vec![], vec![], 3600);
        combined.capabilities = vec![
            Capability {
                id: "cap:cheap:translation".into(),
                name: "translation".into(),
                description: "cheap".into(),
                input: serde_json::json!({}),
                output: serde_json::json!({}),
                price: Some(Price {
                    amount: 0.01,
                    currency: "EUR".into(),
                    model: "per_unit".into(),
                }),
                autonomy: vec!["propose".into()],
            },
            Capability {
                id: "cap:expensive:consulting".into(),
                name: "consulting".into(),
                description: "high-end".into(),
                input: serde_json::json!({}),
                output: serde_json::json!({}),
                price: Some(Price {
                    amount: 500.0,
                    currency: "EUR".into(),
                    model: "fixed".into(),
                }),
                autonomy: vec!["propose".into()],
            },
        ];
        // Re-sign after mutating capabilities.
        combined.resign(&agent);
        reg.announce(combined).unwrap();

        // Query for cheap translation within budget
        let q = Query {
            name: Some("translation".into()),
            max_price: Some(0.05),
            ..Default::default()
        };
        assert_eq!(reg.query(&q).len(), 1);

        // Query for expensive consulting within a small budget -> no hit
        let q2 = Query {
            name: Some("consulting".into()),
            max_price: Some(0.05),
            ..Default::default()
        };
        assert_eq!(reg.query(&q2).len(), 0);

        // No name filter, small budget -> the agent has a cheap capability,
        // so the announcement matches.
        let q3 = Query {
            max_price: Some(0.05),
            ..Default::default()
        };
        assert_eq!(reg.query(&q3).len(), 1);
    }

    #[test]
    fn registry_filters_by_reputation() {
        let mut reg = Registry::new();
        let (a, cap_a) = make_agent("a");
        let (b, cap_b) = make_agent("b");
        announce(&mut reg, &a, cap_a, 3600);
        announce(&mut reg, &b, cap_b, 3600);

        reg.set_reputation(a.did().clone(), 0.95);
        reg.set_reputation(b.did().clone(), 0.60);

        let q = Query {
            min_reputation: Some(0.9),
            ..Default::default()
        };
        let hits = reg.query(&q);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].agent_did, *a.did());
    }

    #[test]
    fn registry_respects_max_results() {
        let mut reg = Registry::new();
        for i in 0..5 {
            let (agent, cap) = make_agent(&format!("agent{i}"));
            announce(&mut reg, &agent, cap, 3600);
        }
        let q = Query {
            max_results: 3,
            ..Default::default()
        };
        assert_eq!(reg.query(&q).len(), 3);
    }
}
