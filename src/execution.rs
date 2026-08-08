//! Execution layer (GAP spec part 04).
//!
//! Execution is where work happens — and where trust is earned. Every
//! delivery carries a proof bundle: deliverable hash, step traces, and
//! attestations, all signed.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};

/// A single step in an execution, with an optional external proof.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub index: u32,
    pub description: String,
    #[serde(default)]
    pub proof: Option<String>,
    pub ts: u64,
}

/// The verifiable proof bundle attached to every delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofBundle {
    pub contract_id: String,
    pub deliverable_hash: String,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub attestations: Vec<Attestation>,
    #[serde(default)]
    pub provider_sig: Option<String>,
}

/// A third-party attestation (auditor, escrow agent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attestation {
    pub verifier: String,
    pub verdict: String,
    pub sig: String,
}

impl ProofBundle {
    /// Build and sign a proof bundle for a delivered payload.
    pub fn signed(
        provider: &AgentIdentity,
        contract_id: &str,
        payload: &[u8],
        steps: Vec<Step>,
    ) -> Self {
        let mut b = Self {
            contract_id: contract_id.to_string(),
            deliverable_hash: format!("sha256:{}", crate::sha256_hex(payload)),
            steps,
            attestations: vec![],
            provider_sig: None,
        };
        b.provider_sig = Some(provider.sign(&b.canonical_bytes()).to_hex());
        b
    }

    /// Verify the provider signature and that the hash matches `payload`.
    /// Requires the provider's full identity (convenience for local use).
    pub fn verify(&self, provider: &AgentIdentity, payload: &[u8]) -> Result<()> {
        self.verify_against(provider.did(), payload)
    }

    /// Verify the provider signature against a **DID only** — this is the
    /// distributed form: any verifier who knows the provider's DID can
    /// check the bundle without holding the provider's keys.
    pub fn verify_against(&self, provider_did: &Did, payload: &[u8]) -> Result<()> {
        // 1. hash must match the actual payload
        let expected = format!("sha256:{}", crate::sha256_hex(payload));
        if self.deliverable_hash != expected {
            return Err(Error::Other("deliverable hash mismatch".into()));
        }
        // 2. signature must be valid against the provider's DID
        let sig_hex = self.provider_sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            provider_did,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.provider_sig = None;
        let v = serde_json::to_value(&clone).expect("bundle serializes");
        serde_json::to_vec(&v).expect("bundle serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_bundle_verifies_and_detects_tampering() {
        let provider = AgentIdentity::generate();
        let payload = b"lead: alice@example.com, verified";
        let bundle = ProofBundle::signed(
            &provider,
            "urn:gap:ctr:test",
            payload,
            vec![Step {
                index: 1,
                description: "scrape inbound queue".into(),
                proof: None,
                ts: crate::message::now_unix(),
            }],
        );
        assert!(bundle.verify(&provider, payload).is_ok());
        // tampered payload
        assert!(bundle.verify(&provider, b"lead: mallory@example.com").is_err());
        // Distributed verification by DID only.
        assert!(bundle.verify_against(provider.did(), payload).is_ok());
        // A verifier with the wrong DID fails.
        let stranger = AgentIdentity::generate();
        assert!(bundle.verify_against(stranger.did(), payload).is_err());
    }

    #[test]
    fn proof_bundle_missing_signature_fails() {
        let provider = AgentIdentity::generate();
        let payload = b"data";
        let mut bundle = ProofBundle::signed(
            &provider,
            "urn:gap:ctr:test",
            payload,
            vec![],
        );
        bundle.provider_sig = None;
        assert!(bundle.verify(&provider, payload).is_err());
    }
}
