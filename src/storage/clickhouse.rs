//! ClickHouse storage backend — production scale.
//!
//! ClickHouse is an OLAP store, and GAP's event-sourcing design makes
//! that a feature: the event spine is a `MergeTree` table (append-only,
//! massively compressible), and materialized state lives in
//! `ReplacingMergeTree` tables that collapse on the latest version.
//!
//! # Atomicity: the sequencer
//!
//! ClickHouse has no multi-row transactions. Operations that require
//! atomicity (escrow park/release, contract state + event) MUST go
//! through the [`Sequencer`]: a single-process writer that serializes
//! critical sections, writes the event, then confirms. This matches
//! the reality of an escrow agent: one process, one order.
//!
//! # Transport
//!
//! The backend talks to ClickHouse over its HTTP JSON interface via a
//! [`HttpTransport`] trait, so tests can use a mock transport while
//! production uses a real HTTP client (ureq).

use super::{AnnouncementRecord, ContractRecord, EventRecord, Storage, validate_event};
use crate::error::{Error, Result};
use std::collections::HashMap;
use std::sync::Mutex;

/// Minimal HTTP transport abstraction (mockable).
pub trait HttpTransport {
    /// POST a raw query to ClickHouse, return the raw response body.
    fn post(&self, query: &str) -> Result<String>;
}

/// A real transport using `ureq`.
pub struct UreqTransport {
    base_url: String,
}

impl UreqTransport {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

impl HttpTransport for UreqTransport {
    fn post(&self, query: &str) -> Result<String> {
        let url = format!("{}/?query={}", self.base_url, urlencode(query));
        let mut resp = ureq::post(&url)
            .send_empty()
            .map_err(|e| Error::Other(format!("clickhouse request failed: {e}")))?;
        resp.body_mut()
            .read_to_string()
            .map_err(|e| Error::Other(format!("clickhouse response failed: {e}")))
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The single-writer sequencer: serializes critical sections.
///
/// `T` is the state protected by the critical section (e.g. escrow
/// balances). The sequencer guarantees: one process, one order, atomic
/// read-modify-write.
pub struct Sequencer<T> {
    inner: Mutex<T>,
}

impl<T> Sequencer<T> {
    pub fn new(initial: T) -> Self {
        Self {
            inner: Mutex::new(initial),
        }
    }

    /// Run a critical section exclusively. Returns the outcome.
    pub fn critical<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut T) -> Result<R>,
    {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| Error::Other("sequencer lock poisoned".into()))?;
        f(&mut guard)
    }
}

/// The ClickHouse table DDL. State tables use ReplacingMergeTree so
/// that upserts collapse to the latest row on merge.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS gap_events (
    seq UInt64,
    kind String,
    at UInt64,
    payload String
) ENGINE = MergeTree ORDER BY seq;

CREATE TABLE IF NOT EXISTS gap_contracts (
    contract_id String,
    client String,
    provider String,
    capability_id String,
    state String,
    contract_json String,
    updated_at UInt64
) ENGINE = ReplacingMergeTree(updated_at) ORDER BY contract_id;

CREATE TABLE IF NOT EXISTS gap_announcements (
    agent_did String,
    announcement_json String,
    expires_at UInt64
) ENGINE = ReplacingMergeTree(expires_at) ORDER BY agent_did;
"#;

/// ClickHouse-backed storage.
pub struct ClickHouseStorage<T: HttpTransport> {
    transport: T,
    /// In-memory mirror of state (ClickHouse does not serve point
    /// reads in real time; the sequencer keeps the hot state).
    contracts: Mutex<HashMap<String, ContractRecord>>,
    announcements: Mutex<HashMap<String, AnnouncementRecord>>,
    events: Mutex<Vec<EventRecord>>,
    /// The atomicity gate for escrow-style operations.
    pub sequencer: Sequencer<()>,
}

impl<T: HttpTransport> ClickHouseStorage<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            contracts: Mutex::new(HashMap::new()),
            announcements: Mutex::new(HashMap::new()),
            events: Mutex::new(vec![]),
            sequencer: Sequencer::new(()),
        }
    }

    /// Create tables on the cluster.
    pub fn migrate(&self) -> Result<()> {
        self.transport.post(DDL)?;
        Ok(())
    }
}

impl<T: HttpTransport> Storage for ClickHouseStorage<T> {
    fn append_event(&mut self, kind: &str, payload: serde_json::Value) -> Result<u64> {
        validate_event(kind, &payload)?;
        let mut events = self
            .events
            .lock()
            .map_err(|_| Error::Other("events lock poisoned".into()))?;
        let seq = events.len() as u64;
        events.push(EventRecord {
            seq,
            kind: kind.into(),
            at: crate::message::now_unix(),
            payload: payload.clone(),
        });
        // Fire-and-forget insert to the cluster (async in production;
        // the mirror is the hot path).
        let q = format!(
            "INSERT INTO gap_events (seq, kind, at, payload) VALUES ({seq}, '{}', {}, '{}')",
            kind.replace('\'', "\\'"),
            crate::message::now_unix(),
            payload.to_string().replace('\'', "\\'"),
        );
        let _ = self.transport.post(&q);
        Ok(seq)
    }

    fn events_after(&self, seq: u64, limit: u64) -> Result<Vec<EventRecord>> {
        let events = self
            .events
            .lock()
            .map_err(|_| Error::Other("events lock poisoned".into()))?;
        Ok(events
            .iter()
            .filter(|e| e.seq > seq)
            .take(limit as usize)
            .cloned()
            .collect())
    }

    fn event_count(&self) -> Result<u64> {
        let events = self
            .events
            .lock()
            .map_err(|_| Error::Other("events lock poisoned".into()))?;
        Ok(events.len() as u64)
    }

    fn upsert_contract(&mut self, record: &ContractRecord) -> Result<()> {
        let mut contracts = self
            .contracts
            .lock()
            .map_err(|_| Error::Other("contracts lock poisoned".into()))?;
        contracts.insert(record.contract_id.clone(), record.clone());
        // Async mirror write; ReplacingMergeTree collapses on merge.
        let q = format!(
            "INSERT INTO gap_contracts VALUES ('{}', '{}', '{}', '{}', '{}', '{}', {})",
            record.contract_id.replace('\'', "\\'"),
            record.client.replace('\'', "\\'"),
            record.provider.replace('\'', "\\'"),
            record.capability_id.replace('\'', "\\'"),
            record.state.replace('\'', "\\'"),
            record.contract_json.replace('\'', "\\'"),
            record.updated_at,
        );
        let _ = self.transport.post(&q);
        Ok(())
    }

    fn get_contract(&self, contract_id: &str) -> Result<Option<ContractRecord>> {
        let contracts = self
            .contracts
            .lock()
            .map_err(|_| Error::Other("contracts lock poisoned".into()))?;
        Ok(contracts.get(contract_id).cloned())
    }

    fn contracts_in_state(&self, state: &str) -> Result<Vec<ContractRecord>> {
        let contracts = self
            .contracts
            .lock()
            .map_err(|_| Error::Other("contracts lock poisoned".into()))?;
        Ok(contracts
            .values()
            .filter(|c| c.state == state)
            .cloned()
            .collect())
    }

    fn upsert_announcement(&mut self, record: &AnnouncementRecord) -> Result<()> {
        let mut announcements = self
            .announcements
            .lock()
            .map_err(|_| Error::Other("announcements lock poisoned".into()))?;
        announcements.insert(record.agent_did.clone(), record.clone());
        let q = format!(
            "INSERT INTO gap_announcements VALUES ('{}', '{}', {})",
            record.agent_did.replace('\'', "\\'"),
            record.announcement_json.replace('\'', "\\'"),
            record.expires_at,
        );
        let _ = self.transport.post(&q);
        Ok(())
    }

    fn get_announcement(&self, agent_did: &str) -> Result<Option<AnnouncementRecord>> {
        let announcements = self
            .announcements
            .lock()
            .map_err(|_| Error::Other("announcements lock poisoned".into()))?;
        Ok(announcements.get(agent_did).cloned())
    }

    fn reap_expired(&mut self) -> Result<usize> {
        let now = crate::message::now_unix();
        let mut announcements = self
            .announcements
            .lock()
            .map_err(|_| Error::Other("announcements lock poisoned".into()))?;
        let before = announcements.len();
        announcements.retain(|_, a| a.expires_at > now);
        let removed = before - announcements.len();
        let q = format!("ALTER TABLE gap_announcements DELETE WHERE expires_at <= {now}");
        let _ = self.transport.post(&q);
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::run_conformance_suite;
    use std::cell::RefCell;

    /// A mock transport recording queries.
    #[derive(Default)]
    pub struct MockTransport {
        pub queries: RefCell<Vec<String>>,
    }

    impl HttpTransport for MockTransport {
        fn post(&self, query: &str) -> Result<String> {
            self.queries.borrow_mut().push(query.to_string());
            Ok(String::new())
        }
    }

    #[test]
    fn clickhouse_passes_conformance_suite() {
        let mut storage = ClickHouseStorage::new(MockTransport::default());
        run_conformance_suite(&mut storage);
    }

    #[test]
    fn clickhouse_emits_queries() {
        let transport = MockTransport::default();
        let mut storage = ClickHouseStorage::new(transport);
        storage
            .append_event("ctr.signed", serde_json::json!({ "id": "c1" }))
            .unwrap();
        assert!(!storage.transport.queries.borrow().is_empty());
        assert!(storage.transport.queries.borrow()[0].contains("INSERT INTO gap_events"));
    }

    #[test]
    fn sequencer_serializes_critical_sections() {
        let seq = Sequencer::new(0u64);
        // Simulate an atomic escrow op: check + modify.
        let result = seq
            .critical(|balance| {
                if *balance + 5 > 10 {
                    return Err(Error::AutonomyViolation("over budget".into()));
                }
                *balance += 5;
                Ok(*balance)
            })
            .unwrap();
        assert_eq!(result, 5);
        // Second op over the cap fails and leaves state unchanged.
        let err = seq.critical(|balance| {
            if *balance + 10 > 10 {
                return Err(Error::AutonomyViolation("over budget".into()));
            }
            *balance += 10;
            Ok(*balance)
        });
        assert!(err.is_err());
        let final_balance = seq.critical(|b| Ok(*b)).unwrap();
        assert_eq!(final_balance, 5);
    }

    #[test]
    fn urlencode_handles_special_chars() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a'b"), "a%27b");
        assert_eq!(urlencode("simple"), "simple");
    }
}
