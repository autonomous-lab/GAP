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
            .ok_or_else(|| Error::Other(format!("invalid did prefix: {s}")))?;
        if rest.len() != 64 || !rest.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Other(format!("invalid did body: {s}")));
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
    pk.verify_strict(msg, &sig)
        .map_err(|_| Error::BadSignature)
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

/// A reputation record: append-only, derived from verifiable attestations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reputation {
    /// Number of recorded executions.
    pub executions: u64,
    /// Successful executions (accepted deliverables).
    pub successes: u64,
    /// On-time deliveries.
    pub on_time: u64,
    /// Signed endorsements from counterparties (DID -> note).
    pub endorsements: Vec<(Did, String)>,
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

    /// Success rate in [0.0, 1.0] (1.0 when no executions yet).
    pub fn success_rate(&self) -> f64 {
        if self.executions == 0 {
            1.0
        } else {
            self.successes as f64 / self.executions as f64
        }
    }

    /// Add a signed endorsement.
    pub fn endorse(&mut self, by: Did, note: String) {
        self.endorsements.push((by, note));
    }
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
        assert_eq!(r.success_rate(), 1.0); // no data -> optimistic
        r.record(true, true);
        r.record(true, false);
        r.record(false, false);
        assert_eq!(r.executions, 3);
        assert_eq!(r.successes, 2);
        assert_eq!(r.on_time, 1);
        assert_eq!(r.success_rate(), 2.0 / 3.0);
        r.endorse(
            Did::parse(
                "did:gap:0000000000000000000000000000000000000000000000000000000000000000",
            )
            .unwrap(),
            "good".into(),
        );
        assert_eq!(r.endorsements.len(), 1);
    }
}
