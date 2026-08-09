//! Verifiable credentials (RFC-0005).
//!
//! Signed assertions by third-party issuers about a subject: publisher
//! verification, insurance coverage, professional codes, conformance.
//! Credentials carry weight only if the issuer is itself reputable.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

/// Known credential types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    PublisherVerified,
    InsuranceCoverage,
    ProfessionalCode,
    DataResidency,
    Conformance,
    PrincipalKyc,
}

impl CredentialType {
    pub fn as_str(&self) -> &'static str {
        match self {
            CredentialType::PublisherVerified => "gap.publisher_verified",
            CredentialType::InsuranceCoverage => "gap.insurance_coverage",
            CredentialType::ProfessionalCode => "gap.professional_code",
            CredentialType::DataResidency => "gap.data_residency",
            CredentialType::Conformance => "gap.conformance",
            CredentialType::PrincipalKyc => "gap.principal_kyc",
        }
    }
}

/// A signed credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub credential_id: String,
    pub r#type: CredentialType,
    pub issuer: Did,
    pub subject: Did,
    pub claims: Value,
    pub valid_from: u64,
    pub valid_until: u64,
    #[serde(default)]
    pub revocation_url: Option<String>,
    #[serde(default)]
    pub issuer_sig: Option<String>,
}

impl Credential {
    /// Issue and sign a credential.
    pub fn issue(
        issuer: &AgentIdentity,
        subject: Did,
        r#type: CredentialType,
        claims: Value,
        valid_from: u64,
        valid_until: u64,
    ) -> Self {
        let mut c = Self {
            credential_id: crate::new_id("vc"),
            r#type,
            issuer: issuer.did().clone(),
            subject,
            claims,
            valid_from,
            valid_until,
            revocation_url: None,
            issuer_sig: None,
        };
        c.resign(issuer);
        c
    }

    /// Re-sign after mutation.
    pub fn resign(&mut self, issuer: &AgentIdentity) {
        self.issuer_sig = None;
        let canonical = self.canonical_bytes();
        self.issuer_sig = Some(issuer.sign(&canonical).to_hex());
    }

    /// Verify the issuer signature and validity window.
    pub fn verify(&self, now: u64) -> Result<()> {
        let sig_hex = self.issuer_sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.issuer,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )?;
        if now < self.valid_from || now > self.valid_until {
            return Err(Error::Other("credential outside validity window".into()));
        }
        Ok(())
    }

    /// Check revocation status against a registry.
    pub fn is_revoked(&self, registry: &RevocationRegistry) -> bool {
        registry.is_revoked(&self.credential_id)
    }

    /// Create a projection (selective disclosure): a subset of claims
    /// re-signed by the SUBJECT, carrying the issuer's original
    /// signature hash so verifiers can bind it to the original.
    pub fn project(
        &self,
        subject: &AgentIdentity,
        claims_subset: &[&str],
    ) -> Result<ProjectedCredential> {
        if self.subject != *subject.did() {
            return Err(Error::Unauthorized(
                "only the subject may project a credential".into(),
            ));
        }
        let mut subset = serde_json::Map::new();
        if let Value::Object(map) = &self.claims {
            for key in claims_subset {
                if let Some(v) = map.get(*key) {
                    subset.insert(key.to_string(), v.clone());
                }
            }
        }
        let mut projected = ProjectedCredential {
            original_credential_id: self.credential_id.clone(),
            original_issuer: self.issuer.clone(),
            original_issuer_sig: self.issuer_sig.clone().unwrap_or_default(),
            subject: self.subject.clone(),
            claims_subset: Value::Object(subset),
            projected_at: crate::message::now_unix(),
            subject_sig: None,
        };
        projected.resign(subject);
        Ok(projected)
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.issuer_sig = None;
        let v = serde_json::to_value(&clone).expect("credential serializes");
        serde_json::to_vec(&v).expect("credential serializes")
    }
}

/// A projected credential: subset of claims re-signed by the subject,
/// bound to the original issuer signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectedCredential {
    pub original_credential_id: String,
    pub original_issuer: Did,
    /// The issuer's signature over the ORIGINAL credential (verifies
    /// that this projection derives from a genuine credential).
    pub original_issuer_sig: String,
    pub subject: Did,
    pub claims_subset: Value,
    pub projected_at: u64,
    #[serde(default)]
    pub subject_sig: Option<String>,
}

impl ProjectedCredential {
    pub fn resign(&mut self, subject: &AgentIdentity) {
        self.subject_sig = None;
        let canonical = self.canonical_bytes();
        self.subject_sig = Some(subject.sign(&canonical).to_hex());
    }

    /// Verify the subject's signature over the projection.
    pub fn verify_projection(&self) -> Result<()> {
        let sig_hex = self.subject_sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.subject,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.subject_sig = None;
        let v = serde_json::to_value(&clone).expect("projection serializes");
        serde_json::to_vec(&v).expect("projection serializes")
    }
}

/// A signed revocation registry.
#[derive(Debug, Default)]
pub struct RevocationRegistry {
    revoked: HashSet<String>,
    signer: Option<AgentIdentity>,
}

impl RevocationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the registry's signing identity.
    pub fn attach_signer(&mut self, signer: AgentIdentity) {
        self.signer = Some(signer);
    }

    /// Revoke a credential id (requires a signer).
    pub fn revoke(&mut self, credential_id: &str) -> Result<()> {
        if self.signer.is_none() {
            return Err(Error::Other("registry has no signer".into()));
        }
        self.revoked.insert(credential_id.to_string());
        Ok(())
    }

    pub fn is_revoked(&self, credential_id: &str) -> bool {
        self.revoked.contains(credential_id)
    }

    pub fn revoked_count(&self) -> usize {
        self.revoked.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn credential() -> (AgentIdentity, AgentIdentity, Credential) {
        let issuer = AgentIdentity::generate();
        let subject = AgentIdentity::generate();
        let c = Credential::issue(
            &issuer,
            subject.did().clone(),
            CredentialType::PublisherVerified,
            json!({ "legal_name": "Weather Pro GmbH", "verified": true }),
            crate::message::now_unix() - 10,
            crate::message::now_unix() + 3600,
        );
        (issuer, subject, c)
    }

    #[test]
    fn credential_verifies_and_detects_tampering() {
        let (_, _, c) = credential();
        assert!(c.verify(crate::message::now_unix()).is_ok());

        let mut c2 = credential().2;
        c2.claims = json!({ "legal_name": "EVIL Corp", "verified": true });
        assert!(c2.verify(crate::message::now_unix()).is_err());
    }

    #[test]
    fn credential_expiry_enforced() {
        let issuer = AgentIdentity::generate();
        let subject = AgentIdentity::generate();
        let c = Credential::issue(
            &issuer,
            subject.did().clone(),
            CredentialType::InsuranceCoverage,
            json!({ "coverage_eur": 5000000 }),
            crate::message::now_unix() - 7200,
            crate::message::now_unix() - 3600, // expired
        );
        assert!(c.verify(crate::message::now_unix()).is_err());
    }

    #[test]
    fn projection_allows_selective_disclosure() {
        let (_, subject, c) = credential();
        let projected = c.project(&subject, &["legal_name"]).unwrap();
        projected.verify_projection().unwrap();
        assert!(projected.claims_subset.get("legal_name").is_some());
        assert!(projected.claims_subset.get("verified").is_none());

        // A stranger cannot project the credential.
        let stranger = AgentIdentity::generate();
        assert!(c.project(&stranger, &["legal_name"]).is_err());
    }

    #[test]
    fn revocation_registry_works() {
        let signer = AgentIdentity::generate();
        let mut registry = RevocationRegistry::new();
        registry.attach_signer(signer);
        let (_, _, c) = credential();
        assert!(!registry.is_revoked(&c.credential_id));
        registry.revoke(&c.credential_id).unwrap();
        assert!(registry.is_revoked(&c.credential_id));
        assert!(c.is_revoked(&registry));
    }

    #[test]
    fn registry_requires_signer_to_revoke() {
        let mut registry = RevocationRegistry::new();
        assert!(registry.revoke("urn:gap:vc:x").is_err());
    }

    #[test]
    fn type_names_are_stable() {
        assert_eq!(
            CredentialType::PublisherVerified.as_str(),
            "gap.publisher_verified"
        );
        assert_eq!(CredentialType::Conformance.as_str(), "gap.conformance");
    }
}
