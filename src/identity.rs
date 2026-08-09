//! Identity layer (GAP spec part 01).
//!
//! GAP identifiers are self-certifying: the `did:gap:` string IS the hex
//! encoding of the agent's Ed25519 public key. Whoever holds the private
//! key owns the identity. No registry required.

use crate::error::{Error, Result};
use ed25519_dalek::Signer as _;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A GAP decentralized identifier: `did:gap:<64 hex chars of pubkey>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Did(String);

impl Did {
    /// Build a DID from a raw Ed25519 public key.
    pub fn from_pubkey(pubkey: &[u8; 32]) -> Self {
        Did(format!("did:gap:{}", hex::encode(pubkey)))
    }

    /// Parse and validate a DID string.
    pub fn parse(s: &str) -> Result<Self> {
        let rest = s
            .strip_prefix("did:gap:")
            .ok_or_else(|| Error::InvalidDid(s.to_string()))?;
        if rest.len() != 64 || !rest.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::InvalidDid(s.to_string()));
        }
        Ok(Did(s.to_string()))
    }

    /// The 32-byte public key encoded in this DID.
    pub fn pubkey(&self) -> [u8; 32] {
        let bytes = hex::decode(&self.0[8..]).expect("validated hex");
        bytes.try_into().expect("64 hex chars -> 32 bytes")
    }
}

impl fmt::Display for Did {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A GAP agent identity: keypair + derived DID + reputation log.
#[derive(Clone, Debug)]
pub struct AgentIdentity {
    signing_key: ed25519_dalek::SigningKey,
    did: Did,
    reputation: Reputation,
}

impl AgentIdentity {
    /// Generate a fresh identity (random keypair).
    pub fn generate() -> Self {
        let mut csprng = rand::rngs::OsRng;
        let signing_key = ed25519_dalek::SigningKey::generate(&mut csprng);
        let did = Did::from_pubkey(&signing_key.verifying_key().to_bytes());
        Self {
            signing_key,
            did,
            reputation: Reputation::default(),
        }
    }

    /// Recover an identity from its 32-byte secret seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(seed);
        let did = Did::from_pubkey(&signing_key.verifying_key().to_bytes());
        Self {
            signing_key,
            did,
            reputation: Reputation::default(),
        }
    }

    /// This agent's DID.
    pub fn did(&self) -> &Did {
        &self.did
    }

    /// Sign a message with this agent's key.
    pub fn sign(&self, msg: &[u8]) -> Signature {
        Signature(self.signing_key.sign(msg).to_bytes())
    }

    /// Export the 32-byte Ed25519 seed for node-side key custody.
    ///
    /// This is intentionally explicit: production deployments should
    /// store it in a KMS-backed backend, not log it or return it from
    /// public APIs.
    pub fn seed_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    /// Export the seed as hex for storage backends.
    pub fn seed_hex(&self) -> String {
        hex::encode(self.seed_bytes())
    }

    /// Verify a signature against this agent's own public key.
    pub fn verify(&self, msg: &[u8], sig: &Signature) -> bool {
        verify_signature(&self.did, msg, sig).is_ok()
    }

    /// Access the reputation log.
    pub fn reputation(&self) -> &Reputation {
        &self.reputation
    }

    /// Mutable access to the reputation log (used by meta-agents).
    pub fn reputation_mut(&mut self) -> &mut Reputation {
        &mut self.reputation
    }
}

/// A detached Ed25519 signature (64 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature(pub [u8; 64]);

impl Signature {
    /// Encode as hex.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// Verify that `sig` is a valid signature by the key embedded in `did`
/// over `msg`.
pub fn verify_signature(did: &Did, msg: &[u8], sig: &Signature) -> Result<()> {
    let pk = ed25519_dalek::VerifyingKey::from_bytes(&did.pubkey())
        .map_err(|e| Error::Other(format!("invalid pubkey: {e}")))?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig.0);
    pk.verify_strict(msg, &sig).map_err(|_| Error::BadSignature)
}

/// The signer capability — anything that can sign and verify.
pub trait Signer {
    fn sign(&self, msg: &[u8]) -> Signature;
    fn verify(&self, msg: &[u8], sig: &Signature) -> bool;
}

impl Signer for AgentIdentity {
    fn sign(&self, msg: &[u8]) -> Signature {
        AgentIdentity::sign(self, msg)
    }
    fn verify(&self, msg: &[u8], sig: &Signature) -> bool {
        AgentIdentity::verify(self, msg, sig)
    }
}

/// A signed endorsement: a counterparty's attestation about an agent.
///
/// Spec §1.4 requires endorsements to be *signed* — an unsigned note
/// could be fabricated by whoever holds the reputation record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endorsement {
    /// Who endorses.
    pub by: Did,
    /// Who is endorsed.
    pub subject: Did,
    pub note: String,
    pub at: u64,
    #[serde(default)]
    pub sig: Option<String>,
}

impl Endorsement {
    /// Create and sign an endorsement of `subject` by `endorser`.
    pub fn signed(endorser: &AgentIdentity, subject: Did, note: impl Into<String>) -> Self {
        let mut e = Self {
            by: endorser.did().clone(),
            subject,
            note: note.into(),
            at: crate::message::now_unix(),
            sig: None,
        };
        e.sig = Some(endorser.sign(&e.canonical_bytes()).to_hex());
        e
    }

    /// Verify the endorser's signature.
    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        verify_signature(&self.by, &self.canonical_bytes(), &Signature(sig_bytes))
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig = None;
        let v = serde_json::to_value(&clone).expect("endorsement serializes");
        serde_json::to_vec(&v).expect("endorsement serializes")
    }
}

/// A reputation record: append-only, derived from verifiable attestations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reputation {
    /// Number of recorded executions.
    pub executions: u64,
    /// Successful executions (accepted deliverables).
    pub successes: u64,
    /// On-time deliveries.
    pub on_time: u64,
    /// Signed endorsements from counterparties.
    pub endorsements: Vec<Endorsement>,
}

impl Reputation {
    /// Record an execution outcome.
    pub fn record(&mut self, accepted: bool, on_time: bool) {
        self.executions += 1;
        if accepted {
            self.successes += 1;
        }
        if on_time {
            self.on_time += 1;
        }
    }

    /// Smoothed success rate in (0.0, 1.0): `(successes+1)/(executions+2)`
    /// (Laplace prior). A brand-new agent scores 0.5, not 1.0 — the old
    /// optimistic default let a fresh identity sail past
    /// `min_reputation` filters, which is exactly how a Sybil launders a
    /// bad record. Confidence grows with `executions`; consumers should
    /// also read `executions` (the `n`) before trusting the number.
    pub fn success_rate(&self) -> f64 {
        (self.successes as f64 + 1.0) / (self.executions as f64 + 2.0)
    }

    /// Unsmoothed ratio (1.0 when no executions yet). For display, not
    /// for filtering.
    pub fn raw_success_rate(&self) -> f64 {
        if self.executions == 0 {
            1.0
        } else {
            self.successes as f64 / self.executions as f64
        }
    }

    /// Append a signed endorsement after verifying its signature.
    pub fn endorse(&mut self, endorsement: Endorsement) -> Result<()> {
        endorsement.verify()?;
        self.endorsements.push(endorsement);
        Ok(())
    }
}

/// A key rotation artifact (spec §1.2): the *old* key signs the handover
/// to the new key, so the DID lineage stays verifiable. Reputation
/// follows the lineage, not the key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRotation {
    pub old_did: Did,
    pub new_did: Did,
    pub at: u64,
    #[serde(default)]
    pub old_sig: Option<String>,
}

impl KeyRotation {
    /// Rotate `old` to the identity behind `new_did`, signed by the old key.
    pub fn signed(old: &AgentIdentity, new_did: Did) -> Self {
        let mut r = Self {
            old_did: old.did().clone(),
            new_did,
            at: crate::message::now_unix(),
            old_sig: None,
        };
        r.old_sig = Some(old.sign(&r.canonical_bytes()).to_hex());
        r
    }

    /// Verify the old key's signature over the handover.
    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.old_sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        verify_signature(
            &self.old_did,
            &self.canonical_bytes(),
            &Signature(sig_bytes),
        )
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.old_sig = None;
        let v = serde_json::to_value(&clone).expect("rotation serializes");
        serde_json::to_vec(&v).expect("rotation serializes")
    }
}

/// Verify an unbroken rotation chain from `origin` to `current`.
///
/// Each link must be signed by its `old_did`, and the links must join
/// end-to-end. Returns the number of hops on success.
pub fn verify_rotation_chain(origin: &Did, current: &Did, chain: &[KeyRotation]) -> Result<usize> {
    let mut cursor = origin.clone();
    for link in chain {
        if link.old_did != cursor {
            return Err(Error::UnverifiedSignature(format!(
                "rotation chain break at {cursor}"
            )));
        }
        link.verify()?;
        cursor = link.new_did.clone();
    }
    if &cursor != current {
        return Err(Error::UnverifiedSignature(
            "rotation chain does not reach the current did".into(),
        ));
    }
    Ok(chain.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn did_parse_and_validate() {
        let a = AgentIdentity::generate();
        let parsed = Did::parse(&a.did().to_string()).unwrap();
        assert_eq!(parsed, *a.did());
        assert!(Did::parse("did:gap:short").is_err());
        assert!(Did::parse("did:eth:0x1234").is_err());
        assert!(Did::parse("").is_err());
        assert!(Did::parse("did:gap:zzz").is_err()); // non-hex
    }

    #[test]
    fn identity_recovered_from_seed_is_stable() {
        let seed = [7u8; 32];
        let a = AgentIdentity::from_seed(&seed);
        let b = AgentIdentity::from_seed(&seed);
        assert_eq!(a.did(), b.did());
        // Signatures from the recovered identity verify.
        let sig = a.sign(b"hello");
        assert!(b.verify(b"hello", &sig));
    }

    #[test]
    fn did_embeds_the_public_key() {
        let a = AgentIdentity::generate();
        let pk = a.signing_key.verifying_key().to_bytes();
        assert_eq!(a.did().pubkey(), pk);
    }

    #[test]
    fn reputation_accumulates_correctly() {
        let mut r = Reputation::default();
        // No data: smoothed prior is 0.5 (a fresh identity must NOT
        // pass high min_reputation filters), raw display is 1.0.
        assert_eq!(r.success_rate(), 0.5);
        assert_eq!(r.raw_success_rate(), 1.0);
        r.record(true, true);
        r.record(true, false);
        r.record(false, false);
        assert_eq!(r.executions, 3);
        assert_eq!(r.successes, 2);
        assert_eq!(r.on_time, 1);
        assert_eq!(r.raw_success_rate(), 2.0 / 3.0);
        assert_eq!(r.success_rate(), 3.0 / 5.0);
        // Smoothed rate converges to the raw rate as n grows.
        for _ in 0..1000 {
            r.record(true, true);
        }
        assert!((r.success_rate() - r.raw_success_rate()).abs() < 0.01);
    }

    #[test]
    fn endorsements_must_be_signed_and_verifiable() {
        let endorser = AgentIdentity::generate();
        let subject = AgentIdentity::generate();
        let mut r = Reputation::default();

        let e = Endorsement::signed(&endorser, subject.did().clone(), "reliable partner");
        assert!(e.verify().is_ok());
        r.endorse(e.clone()).unwrap();
        assert_eq!(r.endorsements.len(), 1);

        // Tampered note breaks the signature.
        let mut forged = e.clone();
        forged.note = "terrible partner".into();
        assert!(forged.verify().is_err());
        assert!(r.endorse(forged).is_err());

        // Unsigned endorsement is rejected outright.
        let mut unsigned = e;
        unsigned.sig = None;
        assert!(r.endorse(unsigned).is_err());
    }

    #[test]
    fn key_rotation_chain_verifies_and_detects_breaks() {
        let k0 = AgentIdentity::generate();
        let k1 = AgentIdentity::generate();
        let k2 = AgentIdentity::generate();

        let r01 = KeyRotation::signed(&k0, k1.did().clone());
        let r12 = KeyRotation::signed(&k1, k2.did().clone());
        assert!(r01.verify().is_ok());

        // Full chain k0 -> k1 -> k2.
        let hops = verify_rotation_chain(k0.did(), k2.did(), &[r01.clone(), r12.clone()]).unwrap();
        assert_eq!(hops, 2);

        // Chain out of order breaks.
        assert!(verify_rotation_chain(k0.did(), k2.did(), &[r12.clone(), r01.clone()]).is_err());
        // Chain that does not reach the claimed current did breaks.
        assert!(verify_rotation_chain(k0.did(), k1.did(), &[r01.clone(), r12]).is_err());

        // A rotation forged by a non-holder of the old key fails: the
        // attacker signs with their own key, not k0's.
        let attacker = AgentIdentity::generate();
        let mut forged = KeyRotation::signed(&attacker, attacker.did().clone());
        forged.old_did = k0.did().clone();
        assert!(forged.verify().is_err());
    }
}
