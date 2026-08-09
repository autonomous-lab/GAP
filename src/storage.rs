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
    pub payload: serde_json::Value,
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

/// The storage abstraction. Implementations MUST be safe to call from
/// the runtime; errors map to [`Error`].
pub trait Storage: Send {
    /// Append an event to the spine. Returns its sequence number.
    fn append_event(&mut self, kind: &str, payload: serde_json::Value) -> Result<u64>;

    /// Read events after a sequence number (for replay/audit).
    fn events_after(&self, seq: u64, limit: u64) -> Result<Vec<EventRecord>>;

    /// Total number of events persisted.
    fn event_count(&self) -> Result<u64>;

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
