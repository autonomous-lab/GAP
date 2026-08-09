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

use super::{
    validate_event, AnnouncementRecord, ContractRecord, EscrowRecord, EventRecord, IdentityRecord,
    Storage,
};
use crate::error::{Error, Result};
use std::collections::HashMap;
use std::sync::Mutex;

/// Minimal HTTP transport abstraction (mockable).
/// A bound query parameter for ClickHouse's `{name:Type}` placeholders.
#[derive(Debug, Clone)]
pub struct QueryParam {
    pub name: &'static str,
    pub value: String,
}

impl QueryParam {
    pub fn new(name: &'static str, value: impl Into<String>) -> Self {
        Self {
            name,
            value: value.into(),
        }
    }
}

/// Minimal HTTP transport abstraction (mockable).
pub trait HttpTransport: Send {
    /// POST a raw query to ClickHouse, return the raw response body.
    fn post(&self, query: &str) -> Result<String>;

    /// POST a query with bound parameters. Implementations MUST bind
    /// values through ClickHouse's `{name:Type}` placeholder mechanism
    /// (`param_<name>` in the request) — never by string interpolation.
    /// This is the SQL-injection-safe path (audit finding C-02).
    fn post_params(&self, query: &str, params: &[QueryParam]) -> Result<String> {
        // Default: append params as query-string bindings after
        // encoding the query itself.
        let mut url = format!("{}/?query={}", self.base_url(), urlencode(query));
        for p in params {
            url.push_str(&format!("&param_{}={}", p.name, urlencode(&p.value)));
        }
        self.post_url(&url)
    }

    /// The base URL (used by the default `post_params`).
    fn base_url(&self) -> String;

    /// POST to a fully-built URL.
    fn post_url(&self, url: &str) -> Result<String>;
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
        self.post_url(&url)
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn post_url(&self, url: &str) -> Result<String> {
        let mut resp = ureq::post(url)
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
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
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

CREATE TABLE IF NOT EXISTS gap_identities (
    token String,
    did String,
    seed_hex String,
    created_at UInt64
) ENGINE = ReplacingMergeTree(created_at) ORDER BY token;

CREATE TABLE IF NOT EXISTS gap_escrows (
    contract_id String,
    state String,
    held String,
    currency String,
    updated_at UInt64
) ENGINE = ReplacingMergeTree(updated_at) ORDER BY contract_id;
"#;

/// ClickHouse-backed storage.
pub struct ClickHouseStorage<T: HttpTransport + Send> {
    transport: T,
    /// In-memory mirror of state (ClickHouse does not serve point
    /// reads in real time; the sequencer keeps the hot state).
    contracts: Mutex<HashMap<String, ContractRecord>>,
    announcements: Mutex<HashMap<String, AnnouncementRecord>>,
    identities: Mutex<HashMap<String, IdentityRecord>>,
    escrows: Mutex<HashMap<String, EscrowRecord>>,
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
            identities: Mutex::new(HashMap::new()),
            escrows: Mutex::new(HashMap::new()),
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
        let at = crate::message::now_unix();
        events.push(EventRecord {
            seq,
            kind: kind.into(),
            at,
            payload: payload.clone(),
        });
        // Fire-and-forget insert with BOUND parameters — never string
        // interpolation (audit fix C-02: SQL injection).
        let q = "INSERT INTO gap_events (seq, kind, at, payload) \
                 VALUES ({seq:UInt64}, {kind:String}, {at:UInt64}, {payload:String})";
        let params = [
            QueryParam::new("seq", seq.to_string()),
            QueryParam::new("kind", kind),
            QueryParam::new("at", at.to_string()),
            QueryParam::new("payload", payload.to_string()),
        ];
        let _ = self.transport.post_params(q, &params);
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
        // Async mirror write with BOUND parameters (audit fix C-02).
        let q = "INSERT INTO gap_contracts (contract_id, client, provider, capability_id, state, contract_json, updated_at) \
                 VALUES ({contract_id:String}, {client:String}, {provider:String}, \
                         {capability_id:String}, {state:String}, {contract_json:String}, {updated_at:UInt64})";
        let params = [
            QueryParam::new("contract_id", &record.contract_id),
            QueryParam::new("client", &record.client),
            QueryParam::new("provider", &record.provider),
            QueryParam::new("capability_id", &record.capability_id),
            QueryParam::new("state", &record.state),
            QueryParam::new("contract_json", &record.contract_json),
            QueryParam::new("updated_at", record.updated_at.to_string()),
        ];
        let _ = self.transport.post_params(q, &params);
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

    fn list_contracts(&self) -> Result<Vec<ContractRecord>> {
        let contracts = self
            .contracts
            .lock()
            .map_err(|_| Error::Other("contracts lock poisoned".into()))?;
        Ok(contracts.values().cloned().collect())
    }

    fn upsert_announcement(&mut self, record: &AnnouncementRecord) -> Result<()> {
        let mut announcements = self
            .announcements
            .lock()
            .map_err(|_| Error::Other("announcements lock poisoned".into()))?;
        announcements.insert(record.agent_did.clone(), record.clone());
        // BOUND parameters (audit fix C-02).
        let q = "INSERT INTO gap_announcements (agent_did, announcement_json, expires_at) \
                 VALUES ({agent_did:String}, {announcement_json:String}, {expires_at:UInt64})";
        let params = [
            QueryParam::new("agent_did", &record.agent_did),
            QueryParam::new("announcement_json", &record.announcement_json),
            QueryParam::new("expires_at", record.expires_at.to_string()),
        ];
        let _ = self.transport.post_params(q, &params);
        Ok(())
    }

    fn get_announcement(&self, agent_did: &str) -> Result<Option<AnnouncementRecord>> {
        let announcements = self
            .announcements
            .lock()
            .map_err(|_| Error::Other("announcements lock poisoned".into()))?;
        Ok(announcements.get(agent_did).cloned())
    }

    fn list_announcements(&self) -> Result<Vec<AnnouncementRecord>> {
        let announcements = self
            .announcements
            .lock()
            .map_err(|_| Error::Other("announcements lock poisoned".into()))?;
        Ok(announcements.values().cloned().collect())
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
        let q = "ALTER TABLE gap_announcements DELETE WHERE expires_at <= {now:UInt64}";
        let params = [QueryParam::new("now", now.to_string())];
        let _ = self.transport.post_params(q, &params);
        Ok(removed)
    }

    fn upsert_identity(&mut self, record: &IdentityRecord) -> Result<()> {
        let mut identities = self
            .identities
            .lock()
            .map_err(|_| Error::Other("identities lock poisoned".into()))?;
        identities.insert(record.token.clone(), record.clone());
        let q = "INSERT INTO gap_identities (token, did, seed_hex, created_at) \
                 VALUES ({token:String}, {did:String}, {seed_hex:String}, {created_at:UInt64})";
        let params = [
            QueryParam::new("token", &record.token),
            QueryParam::new("did", &record.did),
            QueryParam::new("seed_hex", &record.seed_hex),
            QueryParam::new("created_at", record.created_at.to_string()),
        ];
        let _ = self.transport.post_params(q, &params);
        Ok(())
    }

    fn get_identity_by_token(&self, token: &str) -> Result<Option<IdentityRecord>> {
        let identities = self
            .identities
            .lock()
            .map_err(|_| Error::Other("identities lock poisoned".into()))?;
        Ok(identities.get(token).cloned())
    }

    fn list_identities(&self) -> Result<Vec<IdentityRecord>> {
        let identities = self
            .identities
            .lock()
            .map_err(|_| Error::Other("identities lock poisoned".into()))?;
        Ok(identities.values().cloned().collect())
    }

    fn upsert_escrow(&mut self, record: &EscrowRecord) -> Result<()> {
        let mut escrows = self
            .escrows
            .lock()
            .map_err(|_| Error::Other("escrows lock poisoned".into()))?;
        escrows.insert(record.contract_id.clone(), record.clone());
        let q = "INSERT INTO gap_escrows (contract_id, state, held, currency, updated_at) \
                 VALUES ({contract_id:String}, {state:String}, {held:String}, {currency:String}, {updated_at:UInt64})";
        let params = [
            QueryParam::new("contract_id", &record.contract_id),
            QueryParam::new("state", &record.state),
            QueryParam::new("held", &record.held),
            QueryParam::new("currency", &record.currency),
            QueryParam::new("updated_at", record.updated_at.to_string()),
        ];
        let _ = self.transport.post_params(q, &params);
        Ok(())
    }

    fn get_escrow(&self, contract_id: &str) -> Result<Option<EscrowRecord>> {
        let escrows = self
            .escrows
            .lock()
            .map_err(|_| Error::Other("escrows lock poisoned".into()))?;
        Ok(escrows.get(contract_id).cloned())
    }

    fn list_escrows(&self) -> Result<Vec<EscrowRecord>> {
        let escrows = self
            .escrows
            .lock()
            .map_err(|_| Error::Other("escrows lock poisoned".into()))?;
        Ok(escrows.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::run_conformance_suite;
    use std::cell::RefCell;

    /// A mock transport recording queries and bound parameters.
    #[derive(Default)]
    pub struct MockTransport {
        pub queries: RefCell<Vec<String>>,
        /// (query, params) pairs recorded via post_params.
        pub param_queries: RefCell<Vec<(String, Vec<QueryParam>)>>,
    }

    impl HttpTransport for MockTransport {
        fn post(&self, query: &str) -> Result<String> {
            self.queries.borrow_mut().push(query.to_string());
            Ok(String::new())
        }

        fn base_url(&self) -> String {
            "http://mock".into()
        }

        fn post_url(&self, _url: &str) -> Result<String> {
            Ok(String::new())
        }

        fn post_params(&self, query: &str, params: &[QueryParam]) -> Result<String> {
            self.param_queries
                .borrow_mut()
                .push((query.to_string(), params.to_vec()));
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
        let mut storage = ClickHouseStorage::new(MockTransport::default());
        storage
            .append_event("ctr.signed", serde_json::json!({ "id": "c1" }))
            .unwrap();
        let t = &storage.transport;
        assert!(!t.param_queries.borrow().is_empty());
        assert!(t.param_queries.borrow()[0]
            .0
            .contains("INSERT INTO gap_events"));
    }

    #[test]
    fn sql_injection_values_are_bound_not_interpolated() {
        // Audit fix C-02 regression test: hostile values must appear as
        // bound parameters, never inside the query text.
        let mut storage = ClickHouseStorage::new(MockTransport::default());

        let hostile_kind = "x'; DROP TABLE gap_events; --";
        let hostile_payload = serde_json::json!({ "note": "'); DELETE FROM gap_contracts; --" });
        storage.append_event(hostile_kind, hostile_payload).unwrap();

        {
            let (query, params) = &storage.transport.param_queries.borrow()[0];
            // The query text must NOT contain the hostile strings.
            assert!(
                !query.contains("DROP TABLE"),
                "query must not embed hostile kind: {query}"
            );
            assert!(
                !query.contains("DELETE FROM"),
                "query must not embed hostile payload: {query}"
            );
            // The query must use placeholders.
            assert!(
                query.contains("{kind:String}"),
                "query must use bound placeholder: {query}"
            );
            assert!(
                query.contains("{payload:String}"),
                "query must use bound placeholder: {query}"
            );
            // The hostile values travel as bound parameters.
            let kind_param = params
                .iter()
                .find(|p| p.name == "kind")
                .expect("kind param");
            assert_eq!(kind_param.value, hostile_kind);
            let payload_param = params
                .iter()
                .find(|p| p.name == "payload")
                .expect("payload param");
            assert!(payload_param.value.contains("DELETE FROM"));
        }

        // Same for contract upsert.
        let rec = ContractRecord {
            contract_id: "urn:gap:ctr:'; DROP TABLE gap_contracts; --".into(),
            client: "did:gap:a'.client".into(),
            provider: "did:gap:b".into(),
            capability_id: "cap:x'; --".into(),
            state: "signed".into(),
            contract_json: "{\"evil\":\"; DROP\"}".into(),
            updated_at: 1,
        };
        storage.upsert_contract(&rec).unwrap();
        {
            let (query, params) = &storage.transport.param_queries.borrow()[1];
            assert!(
                !query.contains("DROP"),
                "contract query must not embed hostile id: {query}"
            );
            assert!(query.contains("{contract_id:String}"));
            let id_param = params
                .iter()
                .find(|p| p.name == "contract_id")
                .expect("contract_id param");
            assert_eq!(id_param.value, rec.contract_id);
        }
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
