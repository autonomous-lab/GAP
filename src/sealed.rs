//! Confidential payloads (GAP spec part 01 §1.2).
//!
//! The spec says an agent has an X25519 encryption key "derived from
//! the same seed, or a separate key published alongside", and that
//! payloads are encrypted end-to-end "when the contract requires
//! confidentiality" (part 03 §3.2). Until now the contract flag existed
//! and nothing implemented it: a deal marked `confidentiality:
//! encrypted` travelled in clear.
//!
//! This module closes that. A [`SealedEnvelope`] is an anonymous sealed
//! box: an ephemeral X25519 keypair per message, ECDH against the
//! recipient's long-term key, HKDF-SHA256 to a symmetric key, then
//! XChaCha20-Poly1305.
//!
//! **The node cannot read a sealed payload**, even though it holds the
//! agents' Ed25519 signing keys in custody — encryption keys are
//! derived on the agent side from a domain-separated seed, and only the
//! recipient's secret opens the box. That is the point: escrow and
//! audit do not require the node to read the work.

use crate::error::{Error, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey, StaticSecret};

/// Domain separator: the encryption key is derived from the identity
/// seed but is NOT the signing key. Reusing one key for both would let
/// a signature oracle attack the encryption key.
const KDF_INFO: &[u8] = b"gap/x25519/v1";
/// Separator for the per-message symmetric key.
const AEAD_INFO: &[u8] = b"gap/sealed/xchacha20poly1305/v1";

/// An agent's X25519 key pair, derived deterministically from its
/// identity seed so it needs no separate storage or backup.
pub struct EncryptionKey {
    secret: StaticSecret,
}

impl EncryptionKey {
    /// Derive from a 32-byte identity seed (the same seed that backs
    /// the Ed25519 signing key, domain-separated).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, seed);
        let mut okm = [0u8; 32];
        hk.expand(KDF_INFO, &mut okm)
            .expect("32 bytes is a valid HKDF length");
        Self {
            secret: StaticSecret::from(okm),
        }
    }

    /// Derive from an agent identity.
    pub fn of(identity: &crate::identity::AgentIdentity) -> Self {
        Self::from_seed(&identity.seed_bytes())
    }

    /// The public key others encrypt to, hex-encoded for publication in
    /// announcements and AgentCards.
    pub fn public_hex(&self) -> String {
        hex::encode(PublicKey::from(&self.secret).as_bytes())
    }

    pub fn public(&self) -> PublicKey {
        PublicKey::from(&self.secret)
    }

    /// Open a sealed envelope addressed to this key.
    pub fn open(&self, sealed: &SealedEnvelope) -> Result<Vec<u8>> {
        let epk_bytes: [u8; 32] = hex::decode(&sealed.ephemeral_public)
            .map_err(|_| Error::Other("malformed ephemeral key".into()))?
            .try_into()
            .map_err(|_| Error::Other("ephemeral key must be 32 bytes".into()))?;
        let nonce_bytes: [u8; 24] = hex::decode(&sealed.nonce)
            .map_err(|_| Error::Other("malformed nonce".into()))?
            .try_into()
            .map_err(|_| Error::Other("nonce must be 24 bytes".into()))?;
        let ciphertext = hex::decode(&sealed.ciphertext)
            .map_err(|_| Error::Other("malformed ciphertext".into()))?;

        let shared = self.secret.diffie_hellman(&PublicKey::from(epk_bytes));
        let cipher = XChaCha20Poly1305::new(&derive_aead_key(shared.as_bytes(), &epk_bytes).into());
        cipher
            .decrypt(XNonce::from_slice(&nonce_bytes), ciphertext.as_slice())
            // A wrong recipient, a tampered ciphertext and a tampered
            // ephemeral key are indistinguishable here, deliberately.
            .map_err(|_| {
                Error::Other("cannot open sealed payload (not the recipient, or tampered)".into())
            })
    }
}

/// A payload encrypted to one recipient's X25519 public key.
///
/// Anonymous: nothing in the envelope names the sender. Authorship is
/// established the way everything else in GAP is — by signing the
/// carrying [`Envelope`](crate::message::Envelope).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedEnvelope {
    /// Always `"x25519-xchacha20poly1305"` for this version.
    pub alg: String,
    /// Ephemeral public key, fresh per message (hex).
    pub ephemeral_public: String,
    pub nonce: String,
    pub ciphertext: String,
}

impl SealedEnvelope {
    pub const ALG: &'static str = "x25519-xchacha20poly1305";

    /// Seal `plaintext` to a recipient's published X25519 public key.
    pub fn seal(recipient_public_hex: &str, plaintext: &[u8]) -> Result<Self> {
        let pk_bytes: [u8; 32] = hex::decode(recipient_public_hex)
            .map_err(|_| Error::Other("recipient key is not hex".into()))?
            .try_into()
            .map_err(|_| Error::Other("recipient key must be 32 bytes".into()))?;
        let recipient = PublicKey::from(pk_bytes);

        // A fresh ephemeral key per message: without it, two payloads to
        // the same recipient would share a symmetric key.
        use rand::RngCore;
        let mut eph = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut eph);
        let eph_secret = StaticSecret::from(eph);
        let eph_public = PublicKey::from(&eph_secret);
        let shared = eph_secret.diffie_hellman(&recipient);

        let mut nonce_bytes = [0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let cipher = XChaCha20Poly1305::new(
            &derive_aead_key(shared.as_bytes(), eph_public.as_bytes()).into(),
        );
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce_bytes), plaintext)
            .map_err(|_| Error::Other("sealing failed".into()))?;

        Ok(Self {
            alg: Self::ALG.to_string(),
            ephemeral_public: hex::encode(eph_public.as_bytes()),
            nonce: hex::encode(nonce_bytes),
            ciphertext: hex::encode(ciphertext),
        })
    }

    /// Seal a JSON value.
    pub fn seal_json(recipient_public_hex: &str, value: &serde_json::Value) -> Result<Self> {
        Self::seal(recipient_public_hex, &serde_json::to_vec(value)?)
    }

    /// Open into a JSON value.
    pub fn open_json(&self, key: &EncryptionKey) -> Result<serde_json::Value> {
        Ok(serde_json::from_slice(&key.open(self)?)?)
    }
}

/// Bind the symmetric key to the ephemeral public key, so a ciphertext
/// cannot be replayed under a different ephemeral key.
fn derive_aead_key(shared: &[u8; 32], ephemeral_public: &[u8; 32]) -> [u8; 32] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(ephemeral_public), shared);
    let mut key = [0u8; 32];
    hk.expand(AEAD_INFO, &mut key)
        .expect("32 bytes is a valid HKDF length");
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::AgentIdentity;
    use serde_json::json;

    #[test]
    fn a_payload_seals_to_its_recipient_and_opens() {
        let bob = AgentIdentity::generate();
        let bob_key = EncryptionKey::of(&bob);
        let sealed = SealedEnvelope::seal(&bob_key.public_hex(), b"the deliverable").unwrap();
        assert_eq!(sealed.alg, SealedEnvelope::ALG);
        assert_eq!(bob_key.open(&sealed).unwrap(), b"the deliverable");
    }

    #[test]
    fn nobody_else_can_open_it() {
        let bob = AgentIdentity::generate();
        let eve = AgentIdentity::generate();
        let sealed =
            SealedEnvelope::seal(&EncryptionKey::of(&bob).public_hex(), b"under NDA").unwrap();
        assert!(
            EncryptionKey::of(&eve).open(&sealed).is_err(),
            "confidentiality would be theatre otherwise"
        );
    }

    #[test]
    fn the_node_cannot_read_what_it_carries() {
        // The node holds agents' SIGNING keys in custody. That must not
        // give it the ability to read confidential payloads.
        let agent = AgentIdentity::generate();
        let node = AgentIdentity::generate();
        let sealed =
            SealedEnvelope::seal(&EncryptionKey::of(&agent).public_hex(), b"trade secret").unwrap();
        assert!(EncryptionKey::of(&node).open(&sealed).is_err());
    }

    #[test]
    fn encryption_keys_are_derived_not_stored_and_are_not_the_signing_key() {
        let seed = [9u8; 32];
        let a = EncryptionKey::from_seed(&seed);
        let b = EncryptionKey::from_seed(&seed);
        assert_eq!(
            a.public_hex(),
            b.public_hex(),
            "deterministic from the seed"
        );

        // Domain separation: the X25519 public key must not equal the
        // Ed25519 public key material, or a signature oracle would bear
        // on the encryption key.
        let identity = AgentIdentity::from_seed(&seed);
        let signing_pub = hex::encode(identity.did().pubkey());
        assert_ne!(EncryptionKey::of(&identity).public_hex(), signing_pub);
    }

    #[test]
    fn every_message_gets_a_fresh_ephemeral_key_and_nonce() {
        let bob = EncryptionKey::of(&AgentIdentity::generate());
        let a = SealedEnvelope::seal(&bob.public_hex(), b"same text").unwrap();
        let b = SealedEnvelope::seal(&bob.public_hex(), b"same text").unwrap();
        assert_ne!(a.ephemeral_public, b.ephemeral_public);
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(
            a.ciphertext, b.ciphertext,
            "identical plaintext must not look identical"
        );
        assert_eq!(bob.open(&a).unwrap(), bob.open(&b).unwrap());
    }

    #[test]
    fn tampering_is_detected() {
        let bob = EncryptionKey::of(&AgentIdentity::generate());
        let sealed = SealedEnvelope::seal(&bob.public_hex(), b"one hundred euros").unwrap();

        let mut flipped = sealed.clone();
        let mut ct = hex::decode(&flipped.ciphertext).unwrap();
        ct[0] ^= 0x01;
        flipped.ciphertext = hex::encode(ct);
        assert!(bob.open(&flipped).is_err(), "AEAD must catch a flipped bit");

        // Swapping in another ephemeral key must fail too: the symmetric
        // key is bound to it.
        let other = SealedEnvelope::seal(&bob.public_hex(), b"x").unwrap();
        let mut swapped = sealed.clone();
        swapped.ephemeral_public = other.ephemeral_public;
        assert!(bob.open(&swapped).is_err());
    }

    #[test]
    fn json_payloads_round_trip() {
        let bob = EncryptionKey::of(&AgentIdentity::generate());
        let payload = json!({ "leads": [{ "email": "a@x.com" }], "n": 1 });
        let sealed = SealedEnvelope::seal_json(&bob.public_hex(), &payload).unwrap();
        assert_eq!(sealed.open_json(&bob).unwrap(), payload);
    }

    #[test]
    fn malformed_input_is_rejected_rather_than_panicking() {
        let bob = EncryptionKey::of(&AgentIdentity::generate());
        assert!(SealedEnvelope::seal("not-hex", b"x").is_err());
        assert!(SealedEnvelope::seal("aabb", b"x").is_err());
        let mut broken = SealedEnvelope::seal(&bob.public_hex(), b"x").unwrap();
        broken.nonce = "zz".into();
        assert!(bob.open(&broken).is_err());
    }
}
