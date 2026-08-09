//! Governance layer (GAP spec part 06).
//!
//! Governance is the layer that makes GAP adoptable: autonomy levels are
//! negotiated per contract and enforced by the runtime — never
//! self-declared. Certificates define the certified perimeter; meta-agents
//! supervise and can halt non-compliant agents.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};

/// The three autonomy levels of GAP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutonomyLevel {
    /// Prepare and propose; never commit without human approval.
    Propose = 0,
    /// Execute and notify humans in parallel; spend needs approval.
    ExecuteNotify = 1,
    /// Execute within a certified perimeter; breach → halt.
    ExecuteCertified = 2,
}

impl AutonomyLevel {
    /// Parse from a wire string.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "propose" => Ok(AutonomyLevel::Propose),
            "execute-notify" => Ok(AutonomyLevel::ExecuteNotify),
            "execute-certified" => Ok(AutonomyLevel::ExecuteCertified),
            other => Err(Error::Other(format!("unknown autonomy level: {other}"))),
        }
    }
}

/// A certified perimeter: the machine-readable policy inside which an
/// agent may operate at `execute-certified`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Certificate {
    pub cert_id: String,
    pub agent_did: Did,
    pub granted_by: Did,
    pub scope: Scope,
    pub valid_from: u64,
    pub valid_until: u64,
    #[serde(default)]
    pub grantor_sig: Option<String>,
}

/// The scope of a certified perimeter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scope {
    pub allowed_actions: Vec<String>,
    pub denied_actions: Vec<String>,
    #[serde(default)]
    pub budget_per_day: Option<f64>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub jurisdictions: Vec<String>,
}

impl Certificate {
    /// Issue and sign a certificate (by the principal/meta-agent).
    pub fn issue(
        grantor: &AgentIdentity,
        agent_did: Did,
        scope: Scope,
        valid_from: u64,
        valid_until: u64,
    ) -> Self {
        let mut c = Self {
            cert_id: crate::new_id("cert"),
            agent_did,
            granted_by: grantor.did().clone(),
            scope,
            valid_from,
            valid_until,
            grantor_sig: None,
        };
        c.grantor_sig = Some(grantor.sign(&c.canonical_bytes()).to_hex());
        c
    }

    /// Verify the grantor signature and time validity.
    pub fn verify(&self, now: u64) -> Result<()> {
        if now < self.valid_from || now > self.valid_until {
            return Err(Error::Uncertified(
                "certificate expired or not yet valid".into(),
            ));
        }
        let sig_hex = self
            .grantor_sig
            .as_ref()
            .ok_or(Error::Uncertified("unsigned certificate".into()))?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.granted_by,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }

    /// Does this certificate allow `action`?
    pub fn allows(&self, action: &str) -> bool {
        self.scope.allowed_actions.iter().any(|a| a == action)
            && !self.scope.denied_actions.iter().any(|a| a == action)
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.grantor_sig = None;
        let v = serde_json::to_value(&clone).expect("certificate serializes");
        serde_json::to_vec(&v).expect("certificate serializes")
    }
}

/// A meta-agent: supervises other agents, can alert or halt.
pub struct MetaAgent {
    identity: AgentIdentity,
    /// DID of the supervised agent -> (certificate, last alert)
    supervised: Vec<(Did, Option<Certificate>)>,
}

impl MetaAgent {
    pub fn new(identity: AgentIdentity) -> Self {
        Self {
            identity,
            supervised: vec![],
        }
    }

    pub fn did(&self) -> &Did {
        self.identity.did()
    }

    /// Register a supervised agent with an optional certificate.
    ///
    /// The certificate MUST be for the supervised agent (the meta-agent
    /// refuses mismatched pairings).
    pub fn supervise(&mut self, agent: Did, cert: Option<Certificate>) -> Result<()> {
        if let Some(c) = &cert {
            if c.agent_did != agent {
                return Err(Error::Uncertified(
                    "certificate is for a different agent".into(),
                ));
            }
        }
        self.supervised.push((agent, cert));
        Ok(())
    }

    /// Evaluate whether an action is within the certified perimeter.
    pub fn check_action(&self, agent: &Did, action: &str, now: u64) -> Result<()> {
        let entry = self
            .supervised
            .iter()
            .find(|(d, _)| d == agent)
            .ok_or_else(|| Error::Uncertified("agent not supervised".into()))?;
        let cert = entry
            .1
            .as_ref()
            .ok_or_else(|| Error::AutonomyViolation("no certificate for certified level".into()))?;
        cert.verify(now)?;
        if !cert.allows(action) {
            return Err(Error::AutonomyViolation(format!(
                "action '{}' outside certified perimeter",
                action
            )));
        }
        Ok(())
    }

    /// Issue `gov.halt` — a mandatory stop, honored by compliant runtimes.
    pub fn halt(&self, agent: &Did) -> Result<crate::message::Envelope> {
        use crate::message::{Envelope, Kind};
        use serde_json::json;
        Ok(Envelope::new(
            self.identity.did().clone(),
            agent.clone(),
            Kind::GovHalt,
            json!({ "reason": "policy violation" }),
        )
        .sign(&self.identity))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::now_unix;

    #[test]
    fn certified_perimeter_enforced() {
        let principal = AgentIdentity::generate();
        let agent = AgentIdentity::generate();
        let cert = Certificate::issue(
            &principal,
            agent.did().clone(),
            Scope {
                allowed_actions: vec!["read.inbox".into(), "write.draft".into()],
                denied_actions: vec!["send.external".into()],
                budget_per_day: Some(100.0),
                channels: vec!["slack".into()],
                jurisdictions: vec!["EU".into()],
            },
            now_unix() - 10,
            now_unix() + 3600,
        );

        let mut meta = MetaAgent::new(AgentIdentity::generate());
        meta.supervise(agent.did().clone(), Some(cert)).unwrap();

        assert!(meta
            .check_action(agent.did(), "read.inbox", now_unix())
            .is_ok());
        // denied explicitly
        assert!(meta
            .check_action(agent.did(), "send.external", now_unix())
            .is_err());
        // not in allowed list
        assert!(meta
            .check_action(agent.did(), "delete.records", now_unix())
            .is_err());

        let halt = meta.halt(agent.did()).unwrap();
        assert!(halt.verify().is_ok());

        // Unsupervised agent cannot be checked.
        let stranger = AgentIdentity::generate();
        assert!(meta
            .check_action(stranger.did(), "read.inbox", now_unix())
            .is_err());
    }

    #[test]
    fn meta_agent_rejects_mismatched_certificate() {
        let principal = AgentIdentity::generate();
        let agent_a = AgentIdentity::generate();
        let agent_b = AgentIdentity::generate();
        let cert = Certificate::issue(
            &principal,
            agent_a.did().clone(),
            Scope {
                allowed_actions: vec!["read.inbox".into()],
                denied_actions: vec![],
                budget_per_day: None,
                channels: vec![],
                jurisdictions: vec![],
            },
            now_unix() - 10,
            now_unix() + 3600,
        );
        let mut meta = MetaAgent::new(AgentIdentity::generate());
        // Certificate is for agent_a, but we try to supervise agent_b.
        assert!(meta.supervise(agent_b.did().clone(), Some(cert)).is_err());
    }

    #[test]
    fn autonomy_level_parse_roundtrip() {
        assert_eq!(
            AutonomyLevel::parse("propose").unwrap(),
            AutonomyLevel::Propose
        );
        assert_eq!(
            AutonomyLevel::parse("execute-notify").unwrap(),
            AutonomyLevel::ExecuteNotify
        );
        assert_eq!(
            AutonomyLevel::parse("execute-certified").unwrap(),
            AutonomyLevel::ExecuteCertified
        );
        assert!(AutonomyLevel::parse("nonsense").is_err());
        // Ordering: certified > notify > propose
        assert!(AutonomyLevel::ExecuteCertified > AutonomyLevel::ExecuteNotify);
        assert!(AutonomyLevel::ExecuteNotify > AutonomyLevel::Propose);
    }

    #[test]
    fn expired_certificate_fails() {
        let principal = AgentIdentity::generate();
        let agent = AgentIdentity::generate();
        let cert = Certificate::issue(
            &principal,
            agent.did().clone(),
            Scope {
                allowed_actions: vec!["read.inbox".into()],
                denied_actions: vec![],
                budget_per_day: None,
                channels: vec![],
                jurisdictions: vec![],
            },
            now_unix() - 7200,
            now_unix() - 3600, // expired
        );
        assert!(cert.verify(now_unix()).is_err());
    }
}
