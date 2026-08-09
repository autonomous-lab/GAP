//! Seed vault: encryption-at-rest for node-custodied identity seeds.
//!
//! The node stores agents' Ed25519 seeds (key custody). Without a
//! vault, a copy of the database is a copy of every identity on the
//! node. The vault applies envelope encryption with a master key
//! supplied via `GAP_MASTER_KEY` (64 hex chars, 32 bytes):
//!
//! - stored form: `enc:v1:<nonce-hex>:<ciphertext-hex>` (XChaCha20-Poly1305)
//! - plaintext seeds (legacy rows, or vault-less deployments) pass
//!   through unchanged, so enabling the vault is non-breaking; new
//!   writes are encrypted from that point on.
//!
//! The master key belongs in a KMS or secret store in production; the
//! env var is the interface, not the storage recommendation.

use crate::error::{Error, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

/// Prefix marking an encrypted seed in storage.
const PREFIX: &str = "enc:v1:";

/// A seed-encryption vault keyed by the node master key.
pub struct Vault {
    cipher: XChaCha20Poly1305,
}

impl Vault {
    /// Build a vault from a 32-byte master key.
    pub fn new(master_key: &[u8; 32]) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new(master_key.into()),
        }
    }

    /// Build a vault from a 64-hex-char master key string.
    pub fn from_hex(hex_key: &str) -> Result<Self> {
        let bytes = hex::decode(hex_key.trim())
            .map_err(|_| Error::Other("GAP_MASTER_KEY must be hex".into()))?;
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::Other("GAP_MASTER_KEY must be 32 bytes (64 hex chars)".into()))?;
        Ok(Self::new(&key))
    }

    /// Read `GAP_MASTER_KEY` from the environment. `None` when unset —
    /// the node then stores seeds in plaintext and should warn loudly.
    pub fn from_env() -> Option<Result<Self>> {
        std::env::var("GAP_MASTER_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| Self::from_hex(&v))
    }

    /// Encrypt a seed for storage. Fresh random nonce per call.
    pub fn seal(&self, seed_hex: &str) -> String {
        use rand::RngCore;
        let mut nonce_bytes = [0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from_slice(&nonce_bytes);
        let ct = self
            .cipher
            .encrypt(nonce, seed_hex.as_bytes())
            .expect("XChaCha20-Poly1305 encryption is infallible for small inputs");
        format!("{PREFIX}{}:{}", hex::encode(nonce_bytes), hex::encode(ct))
    }

    /// Decrypt a stored seed. Plaintext (non-`enc:`) values pass through
    /// so legacy rows keep working after the vault is enabled.
    pub fn open(&self, stored: &str) -> Result<String> {
        let Some(rest) = stored.strip_prefix(PREFIX) else {
            return Ok(stored.to_string());
        };
        let (nonce_hex, ct_hex) = rest
            .split_once(':')
            .ok_or_else(|| Error::Other("malformed encrypted seed".into()))?;
        let nonce_bytes: [u8; 24] = hex::decode(nonce_hex)
            .map_err(|_| Error::Other("malformed encrypted seed nonce".into()))?
            .try_into()
            .map_err(|_| Error::Other("encrypted seed nonce must be 24 bytes".into()))?;
        let ct = hex::decode(ct_hex)
            .map_err(|_| Error::Other("malformed encrypted seed ciphertext".into()))?;
        let pt = self
            .cipher
            .decrypt(XNonce::from_slice(&nonce_bytes), ct.as_slice())
            .map_err(|_| Error::Other("seed decryption failed (wrong GAP_MASTER_KEY?)".into()))?;
        String::from_utf8(pt).map_err(|_| Error::Other("decrypted seed is not utf-8".into()))
    }

    /// Whether a stored value is in encrypted form.
    pub fn is_sealed(stored: &str) -> bool {
        stored.starts_with(PREFIX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> Vault {
        Vault::new(&[7u8; 32])
    }

    #[test]
    fn seal_open_roundtrip() {
        let v = vault();
        let seed = "ab".repeat(32);
        let sealed = v.seal(&seed);
        assert!(Vault::is_sealed(&sealed));
        assert_ne!(sealed, seed);
        assert_eq!(v.open(&sealed).unwrap(), seed);
    }

    #[test]
    fn nonce_is_fresh_per_seal() {
        let v = vault();
        let seed = "cd".repeat(32);
        assert_ne!(v.seal(&seed), v.seal(&seed));
    }

    #[test]
    fn plaintext_passes_through() {
        let v = vault();
        let legacy = "ef".repeat(32);
        assert_eq!(v.open(&legacy).unwrap(), legacy);
    }

    #[test]
    fn wrong_key_fails_closed() {
        let sealed = vault().seal(&"12".repeat(32));
        let other = Vault::new(&[9u8; 32]);
        assert!(other.open(&sealed).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let v = vault();
        let mut sealed = v.seal(&"34".repeat(32));
        // Flip the last hex digit of the ciphertext.
        let last = sealed.pop().unwrap();
        sealed.push(if last == '0' { '1' } else { '0' });
        assert!(v.open(&sealed).is_err());
    }

    #[test]
    fn from_hex_validates_key_shape() {
        assert!(Vault::from_hex(&"aa".repeat(32)).is_ok());
        assert!(Vault::from_hex("deadbeef").is_err());
        assert!(Vault::from_hex("zz").is_err());
    }
}
