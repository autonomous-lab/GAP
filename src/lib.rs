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

pub mod agentcard;
pub mod amount;
pub mod artifact;
pub mod compliance;
pub mod conformance;
pub mod contract;
pub mod credential;
pub mod custody;
pub mod delegation;
pub mod delivery;
pub mod deposit;
pub mod discovery;
pub mod error;
pub mod execution;
pub mod governance;
pub mod identity;
pub mod irreversibility;
pub mod message;
pub mod onramp;
pub mod payment;
pub mod policy;
pub mod principal;
pub mod receipt_chain;
pub mod relayer;
pub mod runtime;
pub mod sealed;
pub mod server;
pub mod sla;
pub mod storage;
pub mod subscription;
pub mod sybil;
pub mod ui;
pub mod vault;
pub mod verifier;
pub mod workflow;

pub use contract::{Contract, ContractState, Price, Terms};
pub use discovery::{Capability, Query};
pub use error::{Error, Result};
pub use execution::{ProofBundle, Step};
pub use governance::{AutonomyLevel, Certificate};
pub use identity::{AgentIdentity, Did, Endorsement, KeyRotation, Reputation, Signer};
pub use message::{Envelope, Kind, ReplayGuard};
pub use payment::{Escrow, EscrowState, Receipt};
pub use principal::{Principal, PrincipalBinding, Unbind};

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
/// 128 bits of randomness — collision probability is negligible even at
/// billions of ids (audit finding H-02).
pub fn new_id(prefix: &str) -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("urn:gap:{}:{}", prefix, hex::encode(bytes))
}
