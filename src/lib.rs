//! GAP — Geta Agent Protocol.
//!
//! Reference implementation of the GAP specification (v0.1.0), written in Rust.
//!
//! GAP defines how AI agents discover each other, negotiate contracts,
//! execute work, and get paid — across organizations, without a human in
//! every loop. This crate implements the six protocol layers:
//!
//! 1. **Identity** — `did:gap:` identifiers, Ed25519 keys, signatures.
//! 2. **Discovery** — capability announcements and queries.
//! 3. **Contracts** — signed, machine-readable service agreements.
//! 4. **Execution** — verifiable delivery with proof bundles.
//! 5. **Payment** — escrow state machine and receipts.
//! 6. **Governance** — autonomy levels, certification, meta-agents.
//!
//! # Example
//!
//! ```no_run
//! use gap::identity::{AgentIdentity, Signer};
//!
//! let alice = AgentIdentity::generate();
//! let msg = b"hello, gap";
//! let sig = alice.sign(msg);
//! assert!(alice.verify(msg, &sig));
//! ```

pub mod contract;
pub mod delegation;
pub mod discovery;
pub mod error;
pub mod execution;
pub mod governance;
pub mod identity;
pub mod message;
pub mod payment;
pub mod policy;
pub mod receipt_chain;
pub mod runtime;

pub use contract::{Contract, ContractState, Price, Terms};
pub use discovery::{Capability, Query};
pub use error::{Error, Result};
pub use execution::{ProofBundle, Step};
pub use governance::{AutonomyLevel, Certificate};
pub use identity::{AgentIdentity, Did, Reputation, Signer};
pub use message::{Envelope, Kind};
pub use payment::{Escrow, EscrowState, Receipt};

/// The protocol name, as carried in every envelope.
pub const PROTOCOL: &str = "gap";

/// The protocol version implemented by this crate.
pub const VERSION: &str = "0.1.0";

/// Compute the SHA-256 digest of a message, hex-encoded.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Generate a random hex identifier with the given prefix, e.g. `urn:gap:ctr:a1b2…`.
pub fn new_id(prefix: &str) -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("urn:gap:{}:{}", prefix, hex::encode(bytes))
}
