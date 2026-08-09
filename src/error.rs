//! Error taxonomy for GAP.

use std::fmt;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// The GAP error taxonomy. Every error maps to a machine-readable code
/// so that wire-level failures are diagnosable by other agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A message was not signed by its declared sender.
    BadSignature,
    /// A message was signed by a key that does not match its `did:gap:`.
    KeyMismatch,
    /// The envelope's `protocol` or `version` field is unsupported.
    UnsupportedVersion(String),
    /// A message referenced an unknown contract.
    UnknownContract(String),
    /// A state transition was attempted out of order.
    InvalidTransition { from: String, to: String },
    /// An autonomy-level guardrail was violated at runtime.
    AutonomyViolation(String),
    /// A contract was executed before both parties signed it.
    UnsignedContract,
    /// Payment was attempted outside the escrow state machine.
    EscrowViolation(String),
    /// A certification (perimeter) is missing, expired, or revoked.
    Uncertified(String),
    /// The envelope's protocol field is not GAP.
    WrongProtocol(String),
    /// The message timestamp is outside the acceptable skew window.
    StaleTimestamp,
    /// The message sender is not authorized for this action.
    Unauthorized(String),
    /// The message references an unknown/forged contract signature.
    UnverifiedSignature(String),
    /// Serialization/deserialization failure (JSON).
    Json(String),
    /// A `message_id` was seen twice inside the freshness window.
    ReplayedMessage(String),
    /// A DID string does not parse as `did:gap:<64 hex chars>`.
    InvalidDid(String),
    /// A wire string does not name a known state.
    UnknownState(String),
    /// Any other failure.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadSignature => write!(f, "bad signature"),
            Error::KeyMismatch => write!(f, "signing key does not match did"),
            Error::UnsupportedVersion(v) => write!(f, "unsupported protocol version: {v}"),
            Error::UnknownContract(id) => write!(f, "unknown contract: {id}"),
            Error::InvalidTransition { from, to } => {
                write!(f, "invalid transition: {from} -> {to}")
            }
            Error::AutonomyViolation(desc) => write!(f, "autonomy violation: {desc}"),
            Error::UnsignedContract => write!(f, "contract is not signed by both parties"),
            Error::EscrowViolation(desc) => write!(f, "escrow violation: {desc}"),
            Error::Uncertified(desc) => write!(f, "uncertified perimeter: {desc}"),
            Error::WrongProtocol(p) => write!(f, "wrong protocol: {p}"),
            Error::StaleTimestamp => write!(f, "stale timestamp"),
            Error::Unauthorized(desc) => write!(f, "unauthorized: {desc}"),
            Error::UnverifiedSignature(desc) => write!(f, "unverified signature: {desc}"),
            Error::Json(e) => write!(f, "json error: {e}"),
            Error::ReplayedMessage(id) => write!(f, "replayed message: {id}"),
            Error::InvalidDid(s) => write!(f, "invalid did: {s}"),
            Error::UnknownState(s) => write!(f, "unknown state: {s}"),
            Error::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_covers_all_variants() {
        assert_eq!(Error::BadSignature.to_string(), "bad signature");
        assert_eq!(
            Error::KeyMismatch.to_string(),
            "signing key does not match did"
        );
        assert_eq!(
            Error::UnsupportedVersion("9.9".into()).to_string(),
            "unsupported protocol version: 9.9"
        );
        assert_eq!(
            Error::UnknownContract("ctr".into()).to_string(),
            "unknown contract: ctr"
        );
        assert_eq!(
            Error::InvalidTransition {
                from: "a".into(),
                to: "b".into()
            }
            .to_string(),
            "invalid transition: a -> b"
        );
        assert_eq!(
            Error::AutonomyViolation("x".into()).to_string(),
            "autonomy violation: x"
        );
        assert_eq!(
            Error::UnsignedContract.to_string(),
            "contract is not signed by both parties"
        );
        assert_eq!(
            Error::EscrowViolation("y".into()).to_string(),
            "escrow violation: y"
        );
        assert_eq!(
            Error::Uncertified("z".into()).to_string(),
            "uncertified perimeter: z"
        );
        assert_eq!(
            Error::WrongProtocol("p".into()).to_string(),
            "wrong protocol: p"
        );
        assert_eq!(Error::StaleTimestamp.to_string(), "stale timestamp");
        assert_eq!(
            Error::Unauthorized("u".into()).to_string(),
            "unauthorized: u"
        );
        assert_eq!(
            Error::UnverifiedSignature("s".into()).to_string(),
            "unverified signature: s"
        );
        assert_eq!(Error::Json("j".into()).to_string(), "json error: j");
        assert_eq!(
            Error::ReplayedMessage("m".into()).to_string(),
            "replayed message: m"
        );
        assert_eq!(Error::InvalidDid("d".into()).to_string(), "invalid did: d");
        assert_eq!(
            Error::UnknownState("s".into()).to_string(),
            "unknown state: s"
        );
        assert_eq!(Error::Other("o".into()).to_string(), "o");
    }

    #[test]
    fn json_error_converts() {
        let e: Error = serde_json::from_str::<serde_json::Value>("{")
            .unwrap_err()
            .into();
        assert!(matches!(e, Error::Json(_)));
    }
}
