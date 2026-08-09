//! Message layer (GAP spec part 00, §0.3).
//!
//! Every exchange in GAP travels inside a signed `Envelope`. The envelope
//! carries addressing, routing, and protocol metadata; the payload is the
//! layer-specific body.

use crate::error::{Error, Result};
use crate::identity::Did;
use crate::{PROTOCOL, VERSION};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The taxonomy of envelope kinds.
///
/// Serialization uses the normative dotted wire form (`"ctr.propose"`,
/// `"pay.park"`, …) via [`Kind::as_str`]/[`Kind::parse`]. The previous
/// derived form serialized `"ctrpropose"` — internally consistent but
/// diverging from the spec taxonomy, which would have broken every
/// cross-implementation exchange (test-vector work surfaced it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    // discovery (part 02)
    CapAnnounce,
    CapQuery,
    CapDeregister,
    // contracts (part 03)
    CtrPropose,
    CtrCounter,
    CtrAccept,
    CtrReject,
    CtrCancel,
    CtrDispute,
    CtrRuling,
    // execution (part 04)
    ExeStart,
    ExeProgress,
    ExeDeliver,
    ExeAccept,
    ExeReject,
    // payment (part 05)
    PayPark,
    PayParked,
    PayRelease,
    PayReleased,
    PayRefund,
    PayDispute,
    PayRuled,
    // governance (part 06)
    GovCertify,
    GovRevoke,
    GovAlert,
    GovHalt,
    // identity (part 01)
    KeyRotate,
    PrincipalBind,
    PrincipalUnbind,
}

impl Kind {
    /// Wire representation (dotted, lowercased).
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::CapAnnounce => "cap.announce",
            Kind::CapQuery => "cap.query",
            Kind::CapDeregister => "cap.deregister",
            Kind::CtrPropose => "ctr.propose",
            Kind::CtrCounter => "ctr.counter",
            Kind::CtrAccept => "ctr.accept",
            Kind::CtrReject => "ctr.reject",
            Kind::CtrCancel => "ctr.cancel",
            Kind::CtrDispute => "ctr.dispute",
            Kind::CtrRuling => "ctr.ruling",
            Kind::ExeStart => "exe.start",
            Kind::ExeProgress => "exe.progress",
            Kind::ExeDeliver => "exe.deliver",
            Kind::ExeAccept => "exe.accept",
            Kind::ExeReject => "exe.reject",
            Kind::PayPark => "pay.park",
            Kind::PayParked => "pay.parked",
            Kind::PayRelease => "pay.release",
            Kind::PayReleased => "pay.released",
            Kind::PayRefund => "pay.refund",
            Kind::PayDispute => "pay.dispute",
            Kind::PayRuled => "pay.ruled",
            Kind::GovCertify => "gov.certify",
            Kind::GovRevoke => "gov.revoke",
            Kind::GovAlert => "gov.alert",
            Kind::GovHalt => "gov.halt",
            Kind::KeyRotate => "key.rotate",
            Kind::PrincipalBind => "principal.bind",
            Kind::PrincipalUnbind => "principal.unbind",
        }
    }

    /// Parse a wire string back into a `Kind`.
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "cap.announce" => Kind::CapAnnounce,
            "cap.query" => Kind::CapQuery,
            "cap.deregister" => Kind::CapDeregister,
            "ctr.propose" => Kind::CtrPropose,
            "ctr.counter" => Kind::CtrCounter,
            "ctr.accept" => Kind::CtrAccept,
            "ctr.reject" => Kind::CtrReject,
            "ctr.cancel" => Kind::CtrCancel,
            "ctr.dispute" => Kind::CtrDispute,
            "ctr.ruling" => Kind::CtrRuling,
            "exe.start" => Kind::ExeStart,
            "exe.progress" => Kind::ExeProgress,
            "exe.deliver" => Kind::ExeDeliver,
            "exe.accept" => Kind::ExeAccept,
            "exe.reject" => Kind::ExeReject,
            "pay.park" => Kind::PayPark,
            "pay.parked" => Kind::PayParked,
            "pay.release" => Kind::PayRelease,
            "pay.released" => Kind::PayReleased,
            "pay.refund" => Kind::PayRefund,
            "pay.dispute" => Kind::PayDispute,
            "pay.ruled" => Kind::PayRuled,
            "gov.certify" => Kind::GovCertify,
            "gov.revoke" => Kind::GovRevoke,
            "gov.alert" => Kind::GovAlert,
            "gov.halt" => Kind::GovHalt,
            "key.rotate" => Kind::KeyRotate,
            "principal.bind" => Kind::PrincipalBind,
            "principal.unbind" => Kind::PrincipalUnbind,
            _ => return None,
        })
    }
}

impl Serialize for Kind {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Kind::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown envelope kind: {s}")))
    }
}

/// A signed protocol envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol: String,
    pub version: String,
    pub message_id: String,
    pub from: Did,
    pub to: Did,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_id: Option<String>,
    pub kind: Kind,
    pub timestamp: u64,
    pub payload: Value,
    /// Ed25519 signature over the canonical serialization of the envelope
    /// body (everything except `signature`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl Envelope {
    /// Create a new unsigned envelope.
    pub fn new(from: Did, to: Did, kind: Kind, payload: Value) -> Self {
        Self {
            protocol: PROTOCOL.to_string(),
            version: VERSION.to_string(),
            message_id: crate::new_id("msg"),
            from,
            to,
            contract_id: None,
            kind,
            timestamp: now_unix(),
            payload,
            signature: None,
        }
    }

    /// Set the contract this envelope refers to.
    pub fn for_contract(mut self, contract_id: impl Into<String>) -> Self {
        self.contract_id = Some(contract_id.into());
        self
    }

    /// The bytes that must be signed (canonical JSON of the body).
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.signature = None;
        // serde_json is deterministic for a given Value ordering; we
        // serialize via a struct to keep field order stable.
        let body = serde_json::to_value(&clone).expect("envelope serializes");
        serde_json::to_vec(&body).expect("envelope serializes")
    }

    /// Sign this envelope with the sender's identity.
    pub fn sign(mut self, sender: &crate::identity::AgentIdentity) -> Self {
        let sig = sender.sign(&self.canonical_bytes());
        self.signature = Some(sig.to_hex());
        self
    }

    /// Verify the envelope's signature against its `from` DID.
    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.signature.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.from,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }

    /// Full receiver-side validation: protocol, version, signature, and
    /// timestamp freshness. This is what a compliant runtime calls before
    /// acting on any incoming message.
    pub fn validate(&self, max_age_secs: u64) -> Result<()> {
        if self.protocol != crate::PROTOCOL {
            return Err(Error::WrongProtocol(self.protocol.clone()));
        }
        if self.version != crate::VERSION {
            return Err(Error::UnsupportedVersion(self.version.clone()));
        }
        self.verify()?;
        let now = now_unix();
        // Reject messages from the future beyond the skew window, and
        // messages too old (replay protection).
        if self.timestamp > now + max_age_secs {
            return Err(Error::StaleTimestamp);
        }
        if now.saturating_sub(self.timestamp) > max_age_secs {
            return Err(Error::StaleTimestamp);
        }
        Ok(())
    }

    /// Decode a payload field into a typed value.
    pub fn decode<T: for<'de> Deserialize<'de>>(&self) -> Result<T> {
        Ok(serde_json::from_value(self.payload.clone())?)
    }
}

/// Test-controlled clock offset shared by `now_unix`, `advance_clock`,
/// and `reset_clock`. A single module-level static: the previous
/// function-scoped statics were three distinct variables, so the test
/// clock silently never moved.
static CLOCK_OFFSET: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Current UNIX timestamp (seconds).
///
/// Wraps [`std::time::SystemTime`] with a test-controlled offset so that
/// TTL expiry, deadlines, and certificate validity windows can be tested
/// deterministically without sleeping.
pub fn now_unix() -> u64 {
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (base + CLOCK_OFFSET.load(std::sync::atomic::Ordering::Relaxed)) as u64
}

/// Advance the test clock by `secs` seconds. Only compiled in tests.
#[cfg(test)]
pub fn advance_clock(secs: u64) {
    CLOCK_OFFSET.fetch_add(secs as i64, std::sync::atomic::Ordering::Relaxed);
}

/// Reset the test clock to wall time. Only compiled in tests.
#[cfg(test)]
pub fn reset_clock() {
    CLOCK_OFFSET.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// The recommended receiver-side freshness window (seconds). Specified
/// in part 00 §0.3: implementations SHOULD reject envelopes older (or
/// more in the future) than this unless a contract negotiates otherwise.
pub const RECOMMENDED_MAX_AGE_SECS: u64 = 300;

/// Replay protection: freshness window + `message_id` deduplication.
///
/// [`Envelope::validate`] alone rejects *stale* messages, but a captured
/// envelope can still be replayed inside the freshness window. A
/// `ReplayGuard` closes that gap: it remembers every accepted
/// `message_id` until the id leaves the window, and rejects duplicates.
///
/// One guard per trust boundary (one per node, or one per counterparty
/// stream). Memory is bounded: entries are pruned as they expire.
#[derive(Debug, Default)]
pub struct ReplayGuard {
    /// message_id -> unix second after which the id can be forgotten.
    seen: std::collections::HashMap<String, u64>,
}

impl ReplayGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Full receiver-side validation *plus* replay rejection.
    ///
    /// Runs [`Envelope::validate`] (protocol, version, signature,
    /// freshness), then rejects the envelope if its `message_id` was
    /// already accepted inside the window.
    pub fn check(&mut self, envelope: &Envelope, max_age_secs: u64) -> Result<()> {
        envelope.validate(max_age_secs)?;
        let now = now_unix();
        // Prune ids that have left the freshness window: a replay of
        // those is already rejected by the timestamp check.
        self.seen.retain(|_, expiry| *expiry > now);
        let expiry = envelope.timestamp.saturating_add(max_age_secs + 1);
        if self
            .seen
            .insert(envelope.message_id.clone(), expiry)
            .is_some()
        {
            return Err(Error::ReplayedMessage(envelope.message_id.clone()));
        }
        Ok(())
    }

    /// Number of message ids currently remembered.
    pub fn tracked(&self) -> usize {
        self.seen.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::AgentIdentity;
    use serde_json::json;

    #[test]
    fn envelope_roundtrip_and_signature() {
        let alice = AgentIdentity::generate();
        let bob = AgentIdentity::generate();
        let env = Envelope::new(
            alice.did().clone(),
            bob.did().clone(),
            Kind::ExeDeliver,
            json!({ "deliverable_hash": "sha256:abc" }),
        )
        .sign(&alice);
        assert!(env.verify().is_ok());
        let wire = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&wire).unwrap();
        assert!(back.verify().is_ok());
        assert_eq!(back.kind, Kind::ExeDeliver);
    }

    #[test]
    fn tampered_envelope_fails() {
        let alice = AgentIdentity::generate();
        let bob = AgentIdentity::generate();
        let mut env = Envelope::new(
            alice.did().clone(),
            bob.did().clone(),
            Kind::CtrPropose,
            json!({ "terms": "x" }),
        )
        .sign(&alice);
        // tamper with the payload after signing
        env.payload = json!({ "terms": "y" });
        assert!(env.verify().is_err());
    }

    #[test]
    fn kind_parse_roundtrip() {
        for kind in [
            Kind::CapAnnounce,
            Kind::CapQuery,
            Kind::CapDeregister,
            Kind::CtrPropose,
            Kind::CtrCounter,
            Kind::CtrAccept,
            Kind::CtrReject,
            Kind::CtrCancel,
            Kind::CtrDispute,
            Kind::CtrRuling,
            Kind::ExeStart,
            Kind::ExeProgress,
            Kind::ExeDeliver,
            Kind::ExeAccept,
            Kind::ExeReject,
            Kind::PayPark,
            Kind::PayParked,
            Kind::PayRelease,
            Kind::PayReleased,
            Kind::PayRefund,
            Kind::PayDispute,
            Kind::PayRuled,
            Kind::GovCertify,
            Kind::GovRevoke,
            Kind::GovAlert,
            Kind::GovHalt,
            Kind::KeyRotate,
            Kind::PrincipalBind,
            Kind::PrincipalUnbind,
        ] {
            assert_eq!(
                Kind::parse(kind.as_str()),
                Some(kind),
                "parse({})",
                kind.as_str()
            );
        }
        assert_eq!(Kind::parse("not.a.kind"), None);
        assert_eq!(Kind::parse(""), None);
    }

    #[test]
    fn envelope_validate_rejects_wrong_protocol_and_version() {
        let alice = AgentIdentity::generate();
        let mut env = Envelope::new(
            alice.did().clone(),
            alice.did().clone(),
            Kind::CapQuery,
            json!({}),
        )
        .sign(&alice);
        assert!(env.validate(300).is_ok());

        env.protocol = "not-gap".into();
        assert!(matches!(env.validate(300), Err(Error::WrongProtocol(_))));
        env.protocol = PROTOCOL.into();

        env.version = "99.0.0".into();
        assert!(matches!(
            env.validate(300),
            Err(Error::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn envelope_validate_rejects_stale_and_future_timestamps() {
        let alice = AgentIdentity::generate();
        let mut env = Envelope::new(
            alice.did().clone(),
            alice.did().clone(),
            Kind::CapQuery,
            json!({}),
        )
        .sign(&alice);
        let now = now_unix();

        // Too old (replay) — re-sign after mutating the timestamp so
        // signature verification passes and the age check is reached.
        env.timestamp = now - 10_000;
        let env2 = env.clone().sign(&alice);
        assert!(matches!(env2.validate(300), Err(Error::StaleTimestamp)));

        // Too far in the future (clock manipulation)
        env.timestamp = now + 10_000;
        let env3 = env.clone().sign(&alice);
        assert!(matches!(env3.validate(300), Err(Error::StaleTimestamp)));
    }

    #[test]
    fn unsigned_envelope_fails_validation() {
        let alice = AgentIdentity::generate();
        let env = Envelope::new(
            alice.did().clone(),
            alice.did().clone(),
            Kind::CapQuery,
            json!({}),
        );
        assert!(env.validate(300).is_err());
    }

    #[test]
    fn envelope_signed_by_wrong_key_fails() {
        let alice = AgentIdentity::generate();
        let mallory = AgentIdentity::generate();
        // Alice's DID in `from`, but signed by Mallory's key.
        let env = Envelope::new(
            alice.did().clone(),
            alice.did().clone(),
            Kind::CapQuery,
            json!({}),
        )
        .sign(&mallory);
        assert!(env.verify().is_err());
    }

    #[test]
    fn tampering_any_field_invalidates_signature() {
        let alice = AgentIdentity::generate();
        let bob = AgentIdentity::generate();
        let base = Envelope::new(
            alice.did().clone(),
            bob.did().clone(),
            Kind::CtrPropose,
            json!({ "terms": "x" }),
        )
        .for_contract("urn:gap:ctr:orig")
        .sign(&alice);

        // Every field, mutated, must break verification.
        let mut e = base.clone();
        e.protocol = "evil".into();
        assert!(e.verify().is_err(), "protocol");

        let mut e = base.clone();
        e.version = "9.9.9".into();
        assert!(e.verify().is_err(), "version");

        let mut e = base.clone();
        e.message_id = "urn:gap:msg:evil".into();
        assert!(e.verify().is_err(), "message_id");

        let mut e = base.clone();
        e.from = bob.did().clone();
        assert!(e.verify().is_err(), "from");

        let mut e = base.clone();
        e.to = alice.did().clone();
        assert!(e.verify().is_err(), "to");

        let mut e = base.clone();
        e.contract_id = Some("urn:gap:ctr:evil".into());
        assert!(e.verify().is_err(), "contract_id");

        let mut e = base.clone();
        e.kind = Kind::CtrAccept;
        assert!(e.verify().is_err(), "kind");

        let mut e = base.clone();
        e.timestamp += 1;
        assert!(e.verify().is_err(), "timestamp");

        let mut e = base.clone();
        e.payload = json!({ "terms": "EVIL" });
        assert!(e.verify().is_err(), "payload");
    }
}
