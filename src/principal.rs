//! Principal binding (GAP spec part 01, §1.3).
//!
//! Every agent MUST be bound to exactly one **Principal** — the legal or
//! natural person on whose behalf it acts. The binding is a bilateral
//! artifact: it is valid only when signed by BOTH the principal's key
//! and the agent's key. Either party can unbind; counterparties must
//! then treat the agent as untrusted for new contracts until a new
//! binding is attested.

use crate::error::{Error, Result};
use crate::governance::AutonomyLevel;
use crate::identity::{verify_signature, AgentIdentity, Did, Signature};
use serde::{Deserialize, Serialize};

/// The legal identity behind an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// `"organization"` or `"person"`.
    pub kind: String,
    pub name: String,
    /// ISO 3166-1 alpha-2 jurisdiction, e.g. `"FR"`.
    pub jurisdiction: String,
    /// Registration id (SIREN, company number, …) or empty for persons.
    pub id: String,
}

/// A bilateral principal-binding attestation.
///
/// Lifecycle: build with [`PrincipalBinding::draft`], collect both
/// signatures with [`sign_as_principal`](Self::sign_as_principal) and
/// [`sign_as_agent`](Self::sign_as_agent), then [`verify`](Self::verify)
/// before trusting it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrincipalBinding {
    /// Artifact type tag, always `"gap/principal-binding"`.
    pub r#type: String,
    pub agent_did: Did,
    /// The principal's signing DID (the key that co-signs the binding).
    pub principal_did: Did,
    pub principal: Principal,
    pub issued_at: u64,
    pub expires_at: u64,
    /// The maximum autonomy the principal grants this agent.
    pub autonomy_grant: AutonomyLevel,
    #[serde(default)]
    pub principal_sig: Option<String>,
    #[serde(default)]
    pub agent_sig: Option<String>,
}

impl PrincipalBinding {
    pub const TYPE: &'static str = "gap/principal-binding";

    /// Draft an unsigned binding valid for `ttl_secs` from now.
    pub fn draft(
        agent_did: Did,
        principal_did: Did,
        principal: Principal,
        autonomy_grant: AutonomyLevel,
        ttl_secs: u64,
    ) -> Self {
        let now = crate::message::now_unix();
        Self {
            r#type: Self::TYPE.to_string(),
            agent_did,
            principal_did,
            principal,
            issued_at: now,
            expires_at: now.saturating_add(ttl_secs),
            autonomy_grant,
            principal_sig: None,
            agent_sig: None,
        }
    }

    /// Sign as the principal. Fails if the signer is not the declared
    /// principal DID.
    pub fn sign_as_principal(&mut self, principal: &AgentIdentity) -> Result<()> {
        if principal.did() != &self.principal_did {
            return Err(Error::Unauthorized(
                "signer is not the declared principal".into(),
            ));
        }
        self.principal_sig = Some(principal.sign(&self.canonical_bytes()).to_hex());
        Ok(())
    }

    /// Sign as the agent (bilateral consent). Fails if the signer is not
    /// the declared agent DID.
    pub fn sign_as_agent(&mut self, agent: &AgentIdentity) -> Result<()> {
        if agent.did() != &self.agent_did {
            return Err(Error::Unauthorized(
                "signer is not the declared agent".into(),
            ));
        }
        self.agent_sig = Some(agent.sign(&self.canonical_bytes()).to_hex());
        Ok(())
    }

    /// Verify both signatures and the validity window at time `now`.
    pub fn verify_at(&self, now: u64) -> Result<()> {
        if self.r#type != Self::TYPE {
            return Err(Error::Other(format!(
                "wrong artifact type: {}",
                self.r#type
            )));
        }
        let body = self.canonical_bytes();
        check_sig(&self.principal_did, &body, self.principal_sig.as_deref())?;
        check_sig(&self.agent_did, &body, self.agent_sig.as_deref())?;
        if now < self.issued_at || now > self.expires_at {
            return Err(Error::Uncertified("principal binding expired".into()));
        }
        Ok(())
    }

    /// Verify both signatures and that the binding is currently valid.
    pub fn verify(&self) -> Result<()> {
        self.verify_at(crate::message::now_unix())
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.principal_sig = None;
        clone.agent_sig = None;
        let v = serde_json::to_value(&clone).expect("binding serializes");
        serde_json::to_vec(&v).expect("binding serializes")
    }
}

/// A unilateral unbind notice (`principal.unbind`): either party may
/// terminate the binding. Counterparties treat the agent as untrusted
/// for new contracts from `at` onward.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unbind {
    pub agent_did: Did,
    pub principal_did: Did,
    /// Who unbinds — must be one of the two parties.
    pub by: Did,
    pub at: u64,
    #[serde(default)]
    pub sig: Option<String>,
}

impl Unbind {
    /// Create and sign an unbind notice for the given binding.
    pub fn signed(binding: &PrincipalBinding, by: &AgentIdentity) -> Result<Self> {
        if by.did() != &binding.agent_did && by.did() != &binding.principal_did {
            return Err(Error::Unauthorized(
                "only the agent or the principal may unbind".into(),
            ));
        }
        let mut u = Self {
            agent_did: binding.agent_did.clone(),
            principal_did: binding.principal_did.clone(),
            by: by.did().clone(),
            at: crate::message::now_unix(),
            sig: None,
        };
        u.sig = Some(by.sign(&u.canonical_bytes()).to_hex());
        Ok(u)
    }

    /// Verify the unbind: signed by one of the binding's two parties.
    pub fn verify(&self) -> Result<()> {
        if self.by != self.agent_did && self.by != self.principal_did {
            return Err(Error::Unauthorized("unbind signer is neither party".into()));
        }
        check_sig(&self.by, &self.canonical_bytes(), self.sig.as_deref())
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig = None;
        let v = serde_json::to_value(&clone).expect("unbind serializes");
        serde_json::to_vec(&v).expect("unbind serializes")
    }
}

fn check_sig(did: &Did, body: &[u8], sig_hex: Option<&str>) -> Result<()> {
    let sig_hex = sig_hex.ok_or(Error::BadSignature)?;
    let sig_bytes: [u8; 64] = hex::decode(sig_hex)
        .map_err(|_| Error::BadSignature)?
        .try_into()
        .map_err(|_| Error::BadSignature)?;
    verify_signature(did, body, &Signature(sig_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geta() -> Principal {
        Principal {
            kind: "organization".into(),
            name: "Geta.Team".into(),
            jurisdiction: "FR".into(),
            id: "FR-123456789".into(),
        }
    }

    fn bound() -> (AgentIdentity, AgentIdentity, PrincipalBinding) {
        let agent = AgentIdentity::generate();
        let principal = AgentIdentity::generate();
        let mut b = PrincipalBinding::draft(
            agent.did().clone(),
            principal.did().clone(),
            geta(),
            AutonomyLevel::ExecuteNotify,
            3600,
        );
        b.sign_as_principal(&principal).unwrap();
        b.sign_as_agent(&agent).unwrap();
        (agent, principal, b)
    }

    #[test]
    fn bilateral_binding_verifies() {
        let (_, _, b) = bound();
        assert!(b.verify().is_ok());
    }

    #[test]
    fn missing_either_signature_fails() {
        let agent = AgentIdentity::generate();
        let principal = AgentIdentity::generate();
        let mut b = PrincipalBinding::draft(
            agent.did().clone(),
            principal.did().clone(),
            geta(),
            AutonomyLevel::Propose,
            3600,
        );
        assert!(b.verify().is_err(), "unsigned");
        b.sign_as_principal(&principal).unwrap();
        assert!(b.verify().is_err(), "agent consent missing");
        b.sign_as_agent(&agent).unwrap();
        assert!(b.verify().is_ok());
    }

    #[test]
    fn wrong_signer_is_rejected() {
        let agent = AgentIdentity::generate();
        let principal = AgentIdentity::generate();
        let stranger = AgentIdentity::generate();
        let mut b = PrincipalBinding::draft(
            agent.did().clone(),
            principal.did().clone(),
            geta(),
            AutonomyLevel::Propose,
            3600,
        );
        assert!(b.sign_as_principal(&stranger).is_err());
        assert!(b.sign_as_agent(&stranger).is_err());
    }

    #[test]
    fn tampered_binding_fails() {
        let (_, _, mut b) = bound();
        b.principal.name = "Evil Corp".into();
        assert!(b.verify().is_err());
    }

    #[test]
    fn expired_binding_fails() {
        let (_, _, b) = bound();
        assert!(b.verify_at(b.expires_at + 1).is_err());
        assert!(b.verify_at(b.issued_at).is_ok());
    }

    #[test]
    fn unbind_by_either_party_verifies() {
        let (agent, principal, b) = bound();
        let u1 = Unbind::signed(&b, &agent).unwrap();
        assert!(u1.verify().is_ok());
        let u2 = Unbind::signed(&b, &principal).unwrap();
        assert!(u2.verify().is_ok());
    }

    #[test]
    fn unbind_by_stranger_is_rejected() {
        let (_, _, b) = bound();
        let stranger = AgentIdentity::generate();
        assert!(Unbind::signed(&b, &stranger).is_err());
        // Forged: stranger claims to be the agent.
        let mut forged = Unbind::signed(&b, &AgentIdentity::generate());
        assert!(forged.is_err() || forged.as_mut().unwrap().verify().is_err());
    }
}
