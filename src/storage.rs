//! Storage layer — the persistence abstraction.
//!
//! GAP's truth lives in signed artifacts, but those artifacts must be
//! persisted somewhere. This module defines the `Storage` trait with
//! two backends:
//!
//! - [`SqliteStorage`] — local development and tests (single file).
//! - [`ClickHouseStorage`] — production scale (append-only events +
//!   materialized state via ReplacingMergeTree).
//!
//! The design follows the hybrid model: the **event log is the spine**
//! (append-only, chained), and **state is derived** from it via
//! materialized views. Backends differ in *how* they store, never in
//! *what* the protocol requires.

pub mod clickhouse;
pub mod sqlite;

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// Accept a payload that arrives as JSON, or as a string CONTAINING
/// JSON.
///
/// ClickHouse stores the payload in a String column and `select` reads
/// rows with `FORMAT JSONEachRow`, so a hydrated row carries
/// `"payload":"{\"contract_id\":...}"` - a string, where an append in
/// the same process carries an object. Nothing complained, because
/// `serde_json::Value` happily holds either.
///
/// It broke three things, all only after a restart and all silently:
/// `GET /v1/contract/{id}` returned ZERO events, because the filter
/// asks `payload.get("contract_id")` and a string has no keys; the
/// audit output changed shape mid-life; and the spine hash recomputed
/// over a different document than the one it was written from, so the
/// chain verified in the process that wrote it and failed in the next
/// one.
fn de_payload<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<serde_json::Value, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    match v {
        // Only re-parse when the string really is JSON. A payload that
        // is genuinely a string stays one.
        serde_json::Value::String(s) => {
            Ok(serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s)))
        }
        other => Ok(other),
    }
}

/// Normalise a payload to the exact form it will be stored in.
///
/// JSON floats have no canonical text form, and serde_json is
/// round-trip correct on VALUES without being byte-stable on STRINGS:
/// `0.9090909090909091` parses to a double whose shortest
/// representation is `0.9090909090909092`. Both denote the same
/// number, so nothing is wrong - but a hash taken before that
/// normalisation and recomputed after it differs, and the spine hashes
/// a document it re-parses on the next boot.
///
/// A reputation score is a float, so `node.reputation.update` broke the
/// chain in production while every value in it was correct. Passing the
/// payload through one parse/serialise cycle before it is stored AND
/// hashed makes the stored bytes a fixed point: the next parse yields a
/// value that serialises to exactly the same string.
pub fn canonical_payload(v: serde_json::Value) -> serde_json::Value {
    serde_json::from_str(&v.to_string()).unwrap_or(v)
}

/// The hash that links one spine event to the previous one.
///
/// Over a canonical form, not `Debug` or a struct layout: the value has
/// to be recomputable by a stranger from the JSON they can read at
/// `/v1/audit`, years from now, in another language. Field order is
/// fixed here and never derived from a map.
pub fn event_hash(
    seq: u64,
    kind: &str,
    at: u64,
    payload: &serde_json::Value,
    prev: &str,
) -> String {
    // Compact, sorted-key JSON is what the rest of the protocol signs;
    // reusing it means one canonicalisation to get wrong instead of two.
    let canonical = serde_json::json!({
        "at": at,
        "kind": kind,
        "payload": payload,
        "prev_hash": prev,
        "seq": seq,
    });
    format!(
        "sha256:{}",
        crate::sha256_hex(canonical.to_string().as_bytes())
    )
}

/// A persisted protocol event (the append-only spine).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    /// Monotonic sequence number (per-store).
    pub seq: u64,
    /// Event kind, e.g. "ctr.signed", "pay.released", "wf.step.accepted".
    pub kind: String,
    /// ISO-ish timestamp (unix seconds).
    pub at: u64,
    /// The event payload (a signed artifact or its reference).
    #[serde(deserialize_with = "de_payload")]
    pub payload: serde_json::Value,
    /// Hash of the previous event in the spine (RFC-0003).
    ///
    /// Empty on the genesis link, and on every event written before the
    /// chain existed. Those older rows CANNOT be chained retroactively:
    /// computing hashes over them now would produce a chain that proves
    /// only that nobody has touched them since this ran, while looking
    /// exactly like one that proves they were never touched at all.
    /// The chain therefore starts where it starts, and the API says so.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prev_hash: String,
    /// This event's own hash, over its canonical form and `prev_hash`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hash: String,
}

/// A persisted contract (materialized state).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractRecord {
    pub contract_id: String,
    pub client: String,
    pub provider: String,
    pub capability_id: String,
    pub state: String,
    /// Full signed contract JSON (for audit and verification).
    pub contract_json: String,
    pub updated_at: u64,
}

/// A persisted announcement (materialized discovery state).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnouncementRecord {
    pub agent_did: String,
    pub announcement_json: String,
    pub expires_at: u64,
}

/// A persisted node-custodied identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityRecord {
    pub token: String,
    pub did: String,
    pub seed_hex: String,
    pub created_at: u64,
}

/// A persisted escrow materialized state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscrowRecord {
    pub contract_id: String,
    pub state: String,
    pub held: String,
    pub currency: String,
    pub updated_at: u64,
}

/// A delivered artifact held by the node on the parties' behalf.
///
/// The protocol's default is that the artifact travels out of band and
/// the node keeps only the digest. That works when the two agents share
/// a channel, and fails completely when they do not: the buyer has no
/// way to fetch what it paid for, and the judge is asked to rule on
/// acceptance criteria with no content to read - which can only ever
/// return `inconclusive`.
///
/// So the node will carry it, when it is small enough to pass inline.
/// `digest` remains authoritative: it is checked against the content at
/// delivery, so this record cannot disagree with what was committed to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliverableRecord {
    pub contract_id: String,
    /// `sha256:<hex>` over the DECODED bytes.
    pub digest: String,
    /// `base64` for binary, `utf8` for text.
    pub encoding: String,
    /// Advisory, provider-declared (e.g. `image/png`).
    pub media_type: String,
    /// The artifact as sent. Empty when only a URI was supplied.
    pub content: String,
    /// Where to fetch it instead, for artifacts too large to inline.
    pub uri: String,
    pub delivered_at: u64,
}

/// A materialized projection the node keeps in memory and must not lose.
///
/// Contracts, escrows, identities and announcements each earned a typed
/// table. Everything else - verdicts, job history, dispute counters,
/// principal vetoes, budgets, subscriptions, escalations - lived only in
/// RAM, so a restart silently discarded it. That is not a cosmetic loss:
///
/// - a veto that evaporates means an operator froze its agent and the
///   node quietly unfroze it;
/// - a daily budget counter that resets grants a fresh allowance to
///   anyone who can trigger a redeploy;
/// - an escalation that disappears strands escrow awaiting a human
///   review nobody can see any more;
/// - a job history that resets makes "reputation is evidence" false.
///
/// One generic table rather than eight typed ones: these are all
/// serde-serializable maps keyed by a string, the access pattern is
/// identical, and eight near-identical schemas is how the next one gets
/// forgotten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateRecord {
    /// Which projection this belongs to, e.g. `vetoes`.
    pub scope: String,
    /// Key within the projection, e.g. an agent DID.
    pub key: String,
    /// The value, JSON-encoded.
    pub value: String,
    pub updated_at: u64,
}

/// The storage abstraction. Implementations MUST be safe to call from
/// the runtime; errors map to [`Error`].
pub trait Storage: Send {
    /// Append an event to the spine. Returns its sequence number.
    fn append_event(&mut self, kind: &str, payload: serde_json::Value) -> Result<u64>;

    /// Read events after a sequence number (for replay/audit).
    fn events_after(&self, seq: u64, limit: u64) -> Result<Vec<EventRecord>>;

    /// Total number of events persisted.
    fn event_count(&self) -> Result<u64>;

    /// The highest sequence on the spine, or 0 when it is empty.
    ///
    /// NOT the same as [`Storage::event_count`], and the difference has
    /// already cost this node once. A count is only a position while the
    /// sequence is contiguous and starts at one; after a rebuild, a gap,
    /// or a partial read it is neither. `append_event` learned that and
    /// derives from the highest sequence - anything else that wants to
    /// point AT the spine must do the same.
    fn head_seq(&self) -> Result<u64> {
        Ok(self
            .events_after(0, u64::MAX)?
            .last()
            .map(|e| e.seq)
            .unwrap_or(0))
    }

    /// Upsert a contract's materialized state.
    fn upsert_contract(&mut self, record: &ContractRecord) -> Result<()>;

    /// Read a contract by id.
    fn get_contract(&self, contract_id: &str) -> Result<Option<ContractRecord>>;

    /// List contracts in a given state.
    fn contracts_in_state(&self, state: &str) -> Result<Vec<ContractRecord>>;

    /// List all materialized contracts.
    fn list_contracts(&self) -> Result<Vec<ContractRecord>>;

    /// Upsert an announcement.
    fn upsert_announcement(&mut self, record: &AnnouncementRecord) -> Result<()>;

    /// Read an announcement by agent DID.
    fn get_announcement(&self, agent_did: &str) -> Result<Option<AnnouncementRecord>>;

    /// List all materialized announcements.
    fn list_announcements(&self) -> Result<Vec<AnnouncementRecord>>;

    /// Withdraw an announcement for good.
    ///
    /// Deregistration used to drop the announcement from memory and
    /// leave the row untouched, so the next restart put the agent back
    /// in the directory. A withdrawal that a redeploy undoes is not a
    /// withdrawal, and the agent that asked for it has no way to know.
    fn delete_announcement(&mut self, agent_did: &str) -> Result<()>;

    /// Remove expired announcements. Returns the number removed.
    fn reap_expired(&mut self) -> Result<usize>;

    /// Upsert a node-custodied identity.
    fn upsert_identity(&mut self, record: &IdentityRecord) -> Result<()>;

    /// Read an identity by bearer token.
    fn get_identity_by_token(&self, token: &str) -> Result<Option<IdentityRecord>>;

    /// List all node-custodied identities.
    fn list_identities(&self) -> Result<Vec<IdentityRecord>>;

    /// Upsert an escrow's materialized state.
    fn upsert_escrow(&mut self, record: &EscrowRecord) -> Result<()>;

    /// Read an escrow by contract id.
    fn get_escrow(&self, contract_id: &str) -> Result<Option<EscrowRecord>>;

    /// List all escrow materialized states.
    fn list_escrows(&self) -> Result<Vec<EscrowRecord>>;

    /// Store a delivered artifact.
    fn upsert_deliverable(&mut self, record: &DeliverableRecord) -> Result<()>;

    /// Read a delivered artifact by contract id.
    fn get_deliverable(&self, contract_id: &str) -> Result<Option<DeliverableRecord>>;

    /// List all delivered artifacts (used to restore state at startup).
    fn list_deliverables(&self) -> Result<Vec<DeliverableRecord>>;

    /// Store one entry of a materialized projection.
    fn upsert_state(&mut self, record: &StateRecord) -> Result<()>;

    /// Every entry in one projection, for restoring it at startup.
    fn list_state(&self, scope: &str) -> Result<Vec<StateRecord>>;

    /// Forget one entry. Used when a veto is lifted, a subscription is
    /// deleted or an escalation is closed - state that outlives its
    /// purpose is state that misleads the next reader.
    fn delete_state(&mut self, scope: &str, key: &str) -> Result<()>;
}

/// Validate an event before persistence.
pub fn validate_event(kind: &str, payload: &serde_json::Value) -> Result<()> {
    if kind.is_empty() {
        return Err(Error::Other("event kind must not be empty".into()));
    }
    if payload.is_null() {
        return Err(Error::Other("event payload must not be null".into()));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;

    /// Run the full storage conformance suite against any backend.
    /// This is the cross-backend guarantee: SQLite and ClickHouse must
    /// behave identically.
    pub fn run_conformance_suite<S: Storage>(storage: &mut S) {
        // 1. Append + count + read-back.
        let seq1 = storage
            .append_event("ctr.signed", serde_json::json!({ "contract_id": "c1" }))
            .unwrap();
        let seq2 = storage
            .append_event("pay.released", serde_json::json!({ "amount": 5.0 }))
            .unwrap();
        assert_eq!(storage.event_count().unwrap(), 2);
        assert_eq!(seq1, 1, "sequences are 1-based so after=0 means everything");
        assert_eq!(seq2, 2);

        // events_after is strictly-after; with 1-based seqs, after=0
        // yields the whole stream.
        let after = storage.events_after(1, 10).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].kind, "pay.released");
        assert_eq!(after[0].seq, 2);
        // After seq 1 -> nothing.
        assert!(storage.events_after(2, 10).unwrap().is_empty());

        // 2. Rejection of invalid events.
        assert!(storage.append_event("", serde_json::json!({})).is_err());
        assert!(storage.append_event("x", serde_json::Value::Null).is_err());

        // 3. Contract upsert + read + state query.
        let rec = ContractRecord {
            contract_id: "urn:gap:ctr:1".into(),
            client: "did:gap:a".into(),
            provider: "did:gap:b".into(),
            capability_id: "cap:x".into(),
            state: "signed".into(),
            contract_json: "{}".into(),
            updated_at: 1000,
        };
        storage.upsert_contract(&rec).unwrap();
        let got = storage.get_contract("urn:gap:ctr:1").unwrap().unwrap();
        assert_eq!(got.state, "signed");
        assert_eq!(storage.contracts_in_state("signed").unwrap().len(), 1);
        assert_eq!(storage.contracts_in_state("accepted").unwrap().len(), 0);

        // Upsert updates state.
        let mut rec2 = rec.clone();
        rec2.state = "accepted".into();
        storage.upsert_contract(&rec2).unwrap();
        assert_eq!(
            storage
                .get_contract("urn:gap:ctr:1")
                .unwrap()
                .unwrap()
                .state,
            "accepted"
        );
        assert_eq!(storage.contracts_in_state("accepted").unwrap().len(), 1);
        assert_eq!(storage.contracts_in_state("signed").unwrap().len(), 0);
        assert_eq!(storage.list_contracts().unwrap().len(), 1);

        // 4. Announcement upsert + read + expiry.
        let ann = AnnouncementRecord {
            agent_did: "did:gap:agent".into(),
            announcement_json: "{}".into(),
            expires_at: crate::message::now_unix() + 100,
        };
        storage.upsert_announcement(&ann).unwrap();
        assert!(storage.get_announcement("did:gap:agent").unwrap().is_some());
        assert_eq!(storage.list_announcements().unwrap().len(), 1);
        // Not expired yet.
        assert_eq!(storage.reap_expired().unwrap(), 0);
        // Expire it and reap.
        let ann2 = AnnouncementRecord {
            expires_at: crate::message::now_unix() - 10,
            ..ann.clone()
        };
        storage.upsert_announcement(&ann2).unwrap();
        assert_eq!(storage.reap_expired().unwrap(), 1);
        assert!(storage.get_announcement("did:gap:agent").unwrap().is_none());

        // 5. Identity persistence.
        let ident = IdentityRecord {
            token: "gat_test".into(),
            did: "did:gap:agent".into(),
            seed_hex: "00".repeat(32),
            created_at: 1000,
        };
        storage.upsert_identity(&ident).unwrap();
        assert_eq!(
            storage
                .get_identity_by_token("gat_test")
                .unwrap()
                .unwrap()
                .did,
            "did:gap:agent"
        );
        assert_eq!(storage.list_identities().unwrap().len(), 1);

        // 6. Escrow persistence.
        let escrow = EscrowRecord {
            contract_id: "urn:gap:ctr:1".into(),
            state: "parked".into(),
            held: "5.000000".into(),
            currency: "EUR".into(),
            updated_at: 1001,
        };
        storage.upsert_escrow(&escrow).unwrap();
        assert_eq!(
            storage.get_escrow("urn:gap:ctr:1").unwrap().unwrap().held,
            "5.000000"
        );
        assert_eq!(storage.list_escrows().unwrap().len(), 1);
    }
}
#[cfg(test)]
mod payload_tests {
    use super::*;

    /// A hydrated ClickHouse row must rebuild the same document the
    /// append wrote, or everything downstream reads a string where it
    /// expects an object.
    #[test]
    fn a_clickhouse_row_rebuilds_its_payload_as_json() {
        // Exactly what FORMAT JSONEachRow emits for a String column.
        let row = r#"{"seq":308,"kind":"ctr.propose","at":1786400000,
            "payload":"{\"contract_id\":\"urn:gap:ctr:abc\"}",
            "prev_hash":"","hash":"sha256:x"}"#;
        let rec: EventRecord = serde_json::from_str(row).unwrap();
        assert!(rec.payload.is_object(), "got {:?}", rec.payload);
        assert_eq!(rec.payload["contract_id"], "urn:gap:ctr:abc");
    }

    /// The shape SQLite and an in-process append produce must survive
    /// untouched.
    #[test]
    fn a_native_json_payload_is_left_alone() {
        let row = r#"{"seq":1,"kind":"ctr.propose","at":1,
            "payload":{"contract_id":"urn:gap:ctr:abc"}}"#;
        let rec: EventRecord = serde_json::from_str(row).unwrap();
        assert_eq!(rec.payload["contract_id"], "urn:gap:ctr:abc");
    }

    /// A payload that is genuinely a string stays a string. Re-parsing
    /// everything that looks vaguely parseable would turn the word
    /// "null" into JSON null and a bare number into an integer.
    #[test]
    fn a_payload_that_is_really_a_string_is_not_reinterpreted() {
        let row = r#"{"seq":1,"kind":"k","at":1,"payload":"not json at all"}"#;
        let rec: EventRecord = serde_json::from_str(row).unwrap();
        assert_eq!(
            rec.payload,
            serde_json::Value::String("not json at all".into())
        );
    }

    /// Both shapes must hash identically, or the chain verifies in the
    /// process that wrote it and fails in the next one - which is
    /// exactly what production did.
    #[test]
    fn both_shapes_hash_to_the_same_link() {
        let native: EventRecord =
            serde_json::from_str(r#"{"seq":9,"kind":"ctr.accept","at":7,"payload":{"a":1}}"#)
                .unwrap();
        let hydrated: EventRecord =
            serde_json::from_str(r#"{"seq":9,"kind":"ctr.accept","at":7,"payload":"{\"a\":1}"}"#)
                .unwrap();
        assert_eq!(
            event_hash(9, "ctr.accept", 7, &native.payload, ""),
            event_hash(9, "ctr.accept", 7, &hydrated.payload, "")
        );
    }
}
