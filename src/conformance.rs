//! Conformance levels & kit (RFC-0011).
//!
//! Five conformance levels (L0–L4) so implementers adopt GAP
//! incrementally. The ConformanceRunner aggregates the existing test
//! modules per level and emits a signed report.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};

/// The five conformance levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Level {
    /// Envelope format + DID identity.
    L0,
    /// + discovery announce/query + AgentCard.
    L1,
    /// + contracts + execution + proof bundles.
    L2,
    /// + escrow + governance/autonomy + policy engine.
    L3,
    /// + tokenomics settlement + delegation + compliance + full accountability.
    L4,
}

impl Level {
    /// Parse from wire string ("L0".."L4").
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "L0" => Ok(Level::L0),
            "L1" => Ok(Level::L1),
            "L2" => Ok(Level::L2),
            "L3" => Ok(Level::L3),
            "L4" => Ok(Level::L4),
            other => Err(Error::Other(format!("unknown conformance level: {other}"))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Level::L0 => "L0",
            Level::L1 => "L1",
            Level::L2 => "L2",
            Level::L3 => "L3",
            Level::L4 => "L4",
        }
    }

    /// The protocol areas required at this level.
    pub fn required_areas(&self) -> &'static [&'static str] {
        match self {
            Level::L0 => &["identity", "message"],
            Level::L1 => &["identity", "message", "discovery", "agentcard"],
            Level::L2 => &[
                "identity",
                "message",
                "discovery",
                "agentcard",
                "contract",
                "execution",
            ],
            Level::L3 => &[
                "identity",
                "message",
                "discovery",
                "agentcard",
                "contract",
                "execution",
                "payment",
                "governance",
                "policy",
            ],
            Level::L4 => &[
                "identity",
                "message",
                "discovery",
                "agentcard",
                "contract",
                "execution",
                "payment",
                "governance",
                "policy",
                "delegation",
                "compliance",
                "receipt_chain",
                "tokenomics",
            ],
        }
    }
}

/// Per-area test results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AreaResult {
    pub area: String,
    pub tests_run: usize,
    pub tests_passed: usize,
}

impl AreaResult {
    pub fn passed(&self) -> bool {
        self.tests_run == self.tests_passed
    }
}

/// A signed conformance report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformanceReport {
    pub report_id: String,
    pub level: Level,
    pub suite_version: String,
    pub per_area: Vec<AreaResult>,
    pub implementation_version: String,
    pub generated_at: u64,
    pub signed_by: Did,
    #[serde(default)]
    pub sig: Option<String>,
}

impl ConformanceReport {
    /// Whether every area passed.
    pub fn all_passed(&self) -> bool {
        !self.per_area.is_empty() && self.per_area.iter().all(|a| a.passed())
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig = None;
        let v = serde_json::to_value(&clone).expect("report serializes");
        serde_json::to_vec(&v).expect("report serializes")
    }

    pub fn sign(mut self, signer: &AgentIdentity) -> Self {
        self.sig = Some(signer.sign(&self.canonical_bytes()).to_hex());
        self
    }

    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.signed_by,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }
}

/// A minimal registry of conformance claims per DID.
#[derive(Debug, Default)]
pub struct ConformanceRegistry {
    claims: std::collections::HashMap<Did, (Level, String)>,
}

impl ConformanceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a verified conformance claim.
    pub fn claim(&mut self, did: Did, level: Level, report_id: &str) {
        self.claims.insert(did, (level, report_id.to_string()));
    }

    pub fn level_of(&self, did: &Did) -> Option<Level> {
        self.claims.get(did).map(|(l, _)| *l)
    }

    /// Downgrade a participant whose measured behaviour diverged from
    /// their declared level (used by SLA tracking, RFC-0012).
    pub fn downgrade(&mut self, did: &Did, to: Level) -> bool {
        if let Some((current, _)) = self.claims.get_mut(did) {
            if to < *current {
                *current = to;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_ordering_and_areas() {
        assert!(Level::L4 > Level::L3);
        assert!(Level::L2 > Level::L1);
        assert_eq!(Level::parse("L3").unwrap(), Level::L3);
        assert!(Level::parse("L9").is_err());
        // Superset property: L2 requires everything L1 does.
        let l1 = Level::L1.required_areas();
        let l2 = Level::L2.required_areas();
        assert!(l1.iter().all(|a| l2.contains(a)));
    }

    #[test]
    fn report_signs_and_verifies() {
        let signer = AgentIdentity::generate();
        let report = ConformanceReport {
            report_id: crate::new_id("conf"),
            level: Level::L3,
            suite_version: "0.2.0".into(),
            per_area: vec![
                AreaResult {
                    area: "identity".into(),
                    tests_run: 10,
                    tests_passed: 10,
                },
                AreaResult {
                    area: "payment".into(),
                    tests_run: 15,
                    tests_passed: 15,
                },
            ],
            implementation_version: "0.1.0".into(),
            generated_at: crate::message::now_unix(),
            signed_by: signer.did().clone(),
            sig: None,
        }
        .sign(&signer);
        assert!(report.verify().is_ok());
        assert!(report.all_passed());

        let mut failed = report.clone();
        failed.per_area[1].tests_passed = 14;
        assert!(!failed.all_passed());
    }

    #[test]
    fn report_tamper_detected() {
        let signer = AgentIdentity::generate();
        let mut report = ConformanceReport {
            report_id: crate::new_id("conf"),
            level: Level::L1,
            suite_version: "0.2.0".into(),
            per_area: vec![],
            implementation_version: "0.1.0".into(),
            generated_at: crate::message::now_unix(),
            signed_by: signer.did().clone(),
            sig: None,
        }
        .sign(&signer);
        report.level = Level::L4; // tamper
        assert!(report.verify().is_err());
    }

    #[test]
    fn registry_tracks_and_downgrades() {
        let agent = AgentIdentity::generate();
        let mut reg = ConformanceRegistry::new();
        reg.claim(agent.did().clone(), Level::L3, "urn:gap:conf:1");
        assert_eq!(reg.level_of(agent.did()), Some(Level::L3));
        // Downgrade to L1 works.
        assert!(reg.downgrade(agent.did(), Level::L1));
        assert_eq!(reg.level_of(agent.did()), Some(Level::L1));
        // Downgrade to a HIGHER level does nothing.
        assert!(!reg.downgrade(agent.did(), Level::L4));
    }
}
