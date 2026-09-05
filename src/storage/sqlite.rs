//! SQLite storage backend — local development and tests.
//!
//! One file, zero configuration, ACID. The state is materialized in
//! regular tables; the event spine is append-only with a monotonic
//! sequence. This is the backend used by the example and the test
//! suite.

use super::{
    validate_event, AnnouncementRecord, ContractRecord, DeliverableRecord, EscrowRecord,
    EventRecord, IdentityRecord, StateRecord, Storage,
};
use crate::error::{Error, Result};
use rusqlite::{params, Connection};

/// SQLite-backed storage.
pub struct SqliteStorage {
    conn: Connection,
    /// In-memory sequence counter: `MAX(seq)` per insert is O(n) and
    /// was the throughput killer under load (the events table grows
    /// unboundedly). Initialized once at open; single-writer per
    /// process, which matches the node architecture (one storage
    /// instance, sequential writes).
    next_seq: u64,
    /// Hash of the last event written, so the next one can link to it
    /// without a SELECT per append.
    tip_hash: String,
}

impl SqliteStorage {
    /// Open (or create) a database at `path`. Use `:memory:` for tests.
    pub fn open(path: &str) -> Result<Self> {
        let conn =
            Connection::open(path).map_err(|e| Error::Other(format!("sqlite open failed: {e}")))?;
        let mut s = Self {
            conn,
            next_seq: 1,
            tip_hash: String::new(),
        };
        s.init()?;
        // One-time O(n) scan to seed the counter (startup only).
        let max: i64 = s
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM events", [], |r| {
                r.get(0)
            })
            .map_err(|e| Error::Other(format!("sqlite seq init failed: {e}")))?;
        // Databases written before the chain existed have neither
        // column. Adding them is additive and idempotent: the error on
        // a second run is "duplicate column name", which is the desired
        // state and not a failure.
        for col in ["prev_hash", "hash"] {
            let _ = s.conn.execute(
                &format!("ALTER TABLE events ADD COLUMN {col} TEXT NOT NULL DEFAULT ''"),
                [],
            );
        }
        s.next_seq = max.max(1) as u64;
        // Resume the chain from whatever is already on disk. An empty
        // tip is correct for a fresh database AND for one written
        // before the chain existed: in both cases the next event opens
        // a new chain rather than pretending to continue one.
        s.tip_hash = s
            .conn
            .query_row(
                "SELECT hash FROM events ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_default();
        Ok(s)
    }

    fn init(&mut self) -> Result<()> {
        self.conn
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS events (
                     seq INTEGER PRIMARY KEY,
                     kind TEXT NOT NULL,
                     at INTEGER NOT NULL,
                     payload TEXT NOT NULL,
                     prev_hash TEXT NOT NULL DEFAULT '',
                     hash TEXT NOT NULL DEFAULT ''
                 );
                 CREATE TABLE IF NOT EXISTS contracts (
                     contract_id TEXT PRIMARY KEY,
                     client TEXT NOT NULL,
                     provider TEXT NOT NULL,
                     capability_id TEXT NOT NULL,
                     state TEXT NOT NULL,
                     contract_json TEXT NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_contracts_state ON contracts(state);
                 CREATE TABLE IF NOT EXISTS announcements (
                     agent_did TEXT PRIMARY KEY,
                     announcement_json TEXT NOT NULL,
                     expires_at INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS identities (
                     token TEXT PRIMARY KEY,
                     did TEXT NOT NULL UNIQUE,
                     seed_hex TEXT NOT NULL,
                     created_at INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS escrows (
                     contract_id TEXT PRIMARY KEY,
                     state TEXT NOT NULL,
                     held TEXT NOT NULL,
                     currency TEXT NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS node_state (
                     scope TEXT NOT NULL,
                     key TEXT NOT NULL,
                     value TEXT NOT NULL,
                     updated_at INTEGER NOT NULL,
                     PRIMARY KEY (scope, key)
                 );
                 CREATE TABLE IF NOT EXISTS deliverables (
                     contract_id TEXT PRIMARY KEY,
                     digest TEXT NOT NULL,
                     encoding TEXT NOT NULL,
                     media_type TEXT NOT NULL,
                     content TEXT NOT NULL,
                     uri TEXT NOT NULL,
                     delivered_at INTEGER NOT NULL
                 );",
            )
            .map_err(|e| Error::Other(format!("sqlite init failed: {e}")))?;
        Ok(())
    }
}

impl Storage for SqliteStorage {
    fn append_event(&mut self, kind: &str, payload: serde_json::Value) -> Result<u64> {
        validate_event(kind, &payload)?;
        // Fix the bytes before they are hashed AND stored, so the next
        // boot re-parses to the same document.
        let payload = crate::storage::canonical_payload(payload);
        let at = crate::message::now_unix() as i64;
        // O(1) sequence from the in-memory counter (see struct docs:
        // MAX(seq) per insert was quadratic under load).
        let seq = self.next_seq;
        self.next_seq += 1;
        let prev = self.tip_hash.clone();
        let hash = crate::storage::event_hash(seq, kind, at as u64, &payload, &prev);
        self.conn
            .execute(
                "INSERT INTO events (seq, kind, at, payload, prev_hash, hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![seq as i64, kind, at, payload.to_string(), prev, hash],
            )
            .map_err(|e| Error::Other(format!("sqlite insert failed: {e}")))?;
        self.tip_hash = hash;
        Ok(seq)
    }

    fn events_after(&self, seq: u64, limit: u64) -> Result<Vec<EventRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, kind, at, payload, prev_hash, hash FROM events \
                 WHERE seq > ?1 ORDER BY seq LIMIT ?2",
            )
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let rows = stmt
            .query_map(params![seq as i64, limit as i64], |row| {
                Ok(EventRecord {
                    seq: row.get::<_, i64>(0)? as u64,
                    kind: row.get(1)?,
                    at: row.get::<_, i64>(2)? as u64,
                    payload: serde_json::from_str(&row.get::<_, String>(3)?).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?,
                    prev_hash: row.get(4).unwrap_or_default(),
                    hash: row.get(5).unwrap_or_default(),
                })
            })
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        let mut out = vec![];
        for row in rows {
            out.push(row.map_err(|e| Error::Other(format!("sqlite row failed: {e}")))?);
        }
        Ok(out)
    }

    fn head_seq(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))
            .map_err(|e| Error::Other(format!("sqlite head_seq failed: {e}")))?;
        Ok(n as u64)
    }

    fn event_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .map_err(|e| Error::Other(format!("sqlite count failed: {e}")))?;
        Ok(n as u64)
    }

    fn upsert_contract(&mut self, record: &ContractRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO contracts (contract_id, client, provider, capability_id, state, contract_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(contract_id) DO UPDATE SET
                     client = excluded.client,
                     provider = excluded.provider,
                     capability_id = excluded.capability_id,
                     state = excluded.state,
                     contract_json = excluded.contract_json,
                     updated_at = excluded.updated_at",
                params![
                    record.contract_id,
                    record.client,
                    record.provider,
                    record.capability_id,
                    record.state,
                    record.contract_json,
                    record.updated_at as i64,
                ],
            )
            .map_err(|e| Error::Other(format!("sqlite upsert contract failed: {e}")))?;
        Ok(())
    }

    fn get_contract(&self, contract_id: &str) -> Result<Option<ContractRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT contract_id, client, provider, capability_id, state, contract_json, updated_at
                 FROM contracts WHERE contract_id = ?1",
            )
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let mut rows = stmt
            .query_map(params![contract_id], |row| {
                Ok(ContractRecord {
                    contract_id: row.get(0)?,
                    client: row.get(1)?,
                    provider: row.get(2)?,
                    capability_id: row.get(3)?,
                    state: row.get(4)?,
                    contract_json: row.get(5)?,
                    updated_at: row.get::<_, i64>(6)? as u64,
                })
            })
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(Error::Other(format!("sqlite row failed: {e}"))),
            None => Ok(None),
        }
    }

    fn contracts_in_state(&self, state: &str) -> Result<Vec<ContractRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT contract_id, client, provider, capability_id, state, contract_json, updated_at
                 FROM contracts WHERE state = ?1",
            )
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let rows = stmt
            .query_map(params![state], |row| {
                Ok(ContractRecord {
                    contract_id: row.get(0)?,
                    client: row.get(1)?,
                    provider: row.get(2)?,
                    capability_id: row.get(3)?,
                    state: row.get(4)?,
                    contract_json: row.get(5)?,
                    updated_at: row.get::<_, i64>(6)? as u64,
                })
            })
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        let mut out = vec![];
        for row in rows {
            out.push(row.map_err(|e| Error::Other(format!("sqlite row failed: {e}")))?);
        }
        Ok(out)
    }

    fn list_contracts(&self) -> Result<Vec<ContractRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT contract_id, client, provider, capability_id, state, contract_json, updated_at
                 FROM contracts ORDER BY updated_at, contract_id",
            )
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ContractRecord {
                    contract_id: row.get(0)?,
                    client: row.get(1)?,
                    provider: row.get(2)?,
                    capability_id: row.get(3)?,
                    state: row.get(4)?,
                    contract_json: row.get(5)?,
                    updated_at: row.get::<_, i64>(6)? as u64,
                })
            })
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        let mut out = vec![];
        for row in rows {
            out.push(row.map_err(|e| Error::Other(format!("sqlite row failed: {e}")))?);
        }
        Ok(out)
    }

    fn upsert_announcement(&mut self, record: &AnnouncementRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO announcements (agent_did, announcement_json, expires_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(agent_did) DO UPDATE SET
                     announcement_json = excluded.announcement_json,
                     expires_at = excluded.expires_at",
                params![
                    record.agent_did,
                    record.announcement_json,
                    record.expires_at as i64,
                ],
            )
            .map_err(|e| Error::Other(format!("sqlite upsert announcement failed: {e}")))?;
        Ok(())
    }

    fn get_announcement(&self, agent_did: &str) -> Result<Option<AnnouncementRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT agent_did, announcement_json, expires_at FROM announcements WHERE agent_did = ?1")
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let mut rows = stmt
            .query_map(params![agent_did], |row| {
                Ok(AnnouncementRecord {
                    agent_did: row.get(0)?,
                    announcement_json: row.get(1)?,
                    expires_at: row.get::<_, i64>(2)? as u64,
                })
            })
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(Error::Other(format!("sqlite row failed: {e}"))),
            None => Ok(None),
        }
    }

    fn delete_announcement(&mut self, agent_did: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM announcements WHERE agent_did = ?1",
                [agent_did],
            )
            .map_err(|e| Error::Other(format!("sqlite delete failed: {e}")))?;
        Ok(())
    }

    fn list_announcements(&self) -> Result<Vec<AnnouncementRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT agent_did, announcement_json, expires_at FROM announcements ORDER BY expires_at, agent_did")
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AnnouncementRecord {
                    agent_did: row.get(0)?,
                    announcement_json: row.get(1)?,
                    expires_at: row.get::<_, i64>(2)? as u64,
                })
            })
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        let mut out = vec![];
        for row in rows {
            out.push(row.map_err(|e| Error::Other(format!("sqlite row failed: {e}")))?);
        }
        Ok(out)
    }

    fn reap_expired(&mut self) -> Result<usize> {
        let now = crate::message::now_unix() as i64;
        let n = self
            .conn
            .execute(
                "DELETE FROM announcements WHERE expires_at <= ?1",
                params![now],
            )
            .map_err(|e| Error::Other(format!("sqlite reap failed: {e}")))?;
        Ok(n)
    }

    fn upsert_identity(&mut self, record: &IdentityRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO identities (token, did, seed_hex, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(token) DO UPDATE SET
                     did = excluded.did,
                     seed_hex = excluded.seed_hex,
                     created_at = excluded.created_at",
                params![
                    record.token,
                    record.did,
                    record.seed_hex,
                    record.created_at as i64,
                ],
            )
            .map_err(|e| Error::Other(format!("sqlite upsert identity failed: {e}")))?;
        Ok(())
    }

    fn get_identity_by_token(&self, token: &str) -> Result<Option<IdentityRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT token, did, seed_hex, created_at FROM identities WHERE token = ?1")
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let mut rows = stmt
            .query_map(params![token], |row| {
                Ok(IdentityRecord {
                    token: row.get(0)?,
                    did: row.get(1)?,
                    seed_hex: row.get(2)?,
                    created_at: row.get::<_, i64>(3)? as u64,
                })
            })
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(Error::Other(format!("sqlite row failed: {e}"))),
            None => Ok(None),
        }
    }

    fn list_identities(&self) -> Result<Vec<IdentityRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT token, did, seed_hex, created_at FROM identities ORDER BY created_at, did",
            )
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(IdentityRecord {
                    token: row.get(0)?,
                    did: row.get(1)?,
                    seed_hex: row.get(2)?,
                    created_at: row.get::<_, i64>(3)? as u64,
                })
            })
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        let mut out = vec![];
        for row in rows {
            out.push(row.map_err(|e| Error::Other(format!("sqlite row failed: {e}")))?);
        }
        Ok(out)
    }

    fn upsert_escrow(&mut self, record: &EscrowRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO escrows (contract_id, state, held, currency, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(contract_id) DO UPDATE SET
                     state = excluded.state,
                     held = excluded.held,
                     currency = excluded.currency,
                     updated_at = excluded.updated_at",
                params![
                    record.contract_id,
                    record.state,
                    record.held,
                    record.currency,
                    record.updated_at as i64,
                ],
            )
            .map_err(|e| Error::Other(format!("sqlite upsert escrow failed: {e}")))?;
        Ok(())
    }

    fn get_escrow(&self, contract_id: &str) -> Result<Option<EscrowRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT contract_id, state, held, currency, updated_at FROM escrows WHERE contract_id = ?1")
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let mut rows = stmt
            .query_map(params![contract_id], |row| {
                Ok(EscrowRecord {
                    contract_id: row.get(0)?,
                    state: row.get(1)?,
                    held: row.get(2)?,
                    currency: row.get(3)?,
                    updated_at: row.get::<_, i64>(4)? as u64,
                })
            })
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(Error::Other(format!("sqlite row failed: {e}"))),
            None => Ok(None),
        }
    }

    fn list_escrows(&self) -> Result<Vec<EscrowRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT contract_id, state, held, currency, updated_at FROM escrows ORDER BY updated_at, contract_id")
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(EscrowRecord {
                    contract_id: row.get(0)?,
                    state: row.get(1)?,
                    held: row.get(2)?,
                    currency: row.get(3)?,
                    updated_at: row.get::<_, i64>(4)? as u64,
                })
            })
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        let mut out = vec![];
        for row in rows {
            out.push(row.map_err(|e| Error::Other(format!("sqlite row failed: {e}")))?);
        }
        Ok(out)
    }

    fn upsert_deliverable(&mut self, record: &DeliverableRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO deliverables
                   (contract_id, digest, encoding, media_type, content, uri, delivered_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(contract_id) DO UPDATE SET
                   digest=excluded.digest, encoding=excluded.encoding,
                   media_type=excluded.media_type, content=excluded.content,
                   uri=excluded.uri, delivered_at=excluded.delivered_at",
                rusqlite::params![
                    record.contract_id,
                    record.digest,
                    record.encoding,
                    record.media_type,
                    record.content,
                    record.uri,
                    record.delivered_at as i64,
                ],
            )
            .map_err(|e| Error::Other(format!("sqlite insert failed: {e}")))?;
        Ok(())
    }

    fn get_deliverable(&self, contract_id: &str) -> Result<Option<DeliverableRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT contract_id, digest, encoding, media_type, content, uri, delivered_at
                 FROM deliverables WHERE contract_id = ?1",
            )
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let mut rows = stmt
            .query_map([contract_id], row_to_deliverable)
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        match rows.next() {
            Some(r) => {
                Ok(Some(r.map_err(|e| {
                    Error::Other(format!("sqlite row failed: {e}"))
                })?))
            }
            None => Ok(None),
        }
    }

    fn upsert_state(&mut self, record: &StateRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO node_state (scope, key, value, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(scope, key) DO UPDATE SET
                   value=excluded.value, updated_at=excluded.updated_at",
                params![
                    record.scope,
                    record.key,
                    record.value,
                    record.updated_at as i64
                ],
            )
            .map_err(|e| Error::Other(format!("sqlite upsert state failed: {e}")))?;
        Ok(())
    }

    fn list_state(&self, scope: &str) -> Result<Vec<StateRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT scope, key, value, updated_at FROM node_state
                 WHERE scope = ?1 ORDER BY updated_at, key",
            )
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let rows = stmt
            .query_map([scope], row_to_state)
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        let mut out = vec![];
        for row in rows {
            out.push(row.map_err(|e| Error::Other(format!("sqlite row failed: {e}")))?);
        }
        Ok(out)
    }

    fn delete_state(&mut self, scope: &str, key: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM node_state WHERE scope = ?1 AND key = ?2",
                params![scope, key],
            )
            .map_err(|e| Error::Other(format!("sqlite delete state failed: {e}")))?;
        Ok(())
    }

    fn list_deliverables(&self) -> Result<Vec<DeliverableRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT contract_id, digest, encoding, media_type, content, uri, delivered_at
                 FROM deliverables ORDER BY delivered_at, contract_id",
            )
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let rows = stmt
            .query_map([], row_to_deliverable)
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        let mut out = vec![];
        for row in rows {
            out.push(row.map_err(|e| Error::Other(format!("sqlite row failed: {e}")))?);
        }
        Ok(out)
    }
}

fn row_to_state(row: &rusqlite::Row) -> rusqlite::Result<StateRecord> {
    Ok(StateRecord {
        scope: row.get(0)?,
        key: row.get(1)?,
        value: row.get(2)?,
        updated_at: row.get::<_, i64>(3)? as u64,
    })
}

fn row_to_deliverable(row: &rusqlite::Row) -> rusqlite::Result<DeliverableRecord> {
    Ok(DeliverableRecord {
        contract_id: row.get(0)?,
        digest: row.get(1)?,
        encoding: row.get(2)?,
        media_type: row.get(3)?,
        content: row.get(4)?,
        uri: row.get(5)?,
        delivered_at: row.get::<_, i64>(6)? as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trap this pins, which cost the public node its settled feed:
    /// a job's `seq` was taken from `event_count()`. A count is only a
    /// position while the sequence is contiguous and starts at one.
    /// After the spine was rebuilt the stored jobs carried sequences
    /// ABOVE the new head, `/v1/activity` paged from the highest of
    /// them, and every settlement made afterwards landed below that
    /// cursor - so the node kept settling and the feed showed nothing
    /// new, even after acceptance.
    #[test]
    fn head_seq_is_a_position_not_a_population() {
        let mut s = SqliteStorage::open(":memory:").unwrap();
        assert_eq!(s.head_seq().unwrap(), 0, "an empty spine has no head");
        for _ in 0..3 {
            s.append_event("ctr.propose", serde_json::json!({ "contract_id": "c" }))
                .unwrap();
        }
        assert_eq!(s.event_count().unwrap(), 3);
        assert_eq!(s.head_seq().unwrap(), 3, "contiguous: they agree");

        // Now make them disagree, the way a rebuild does.
        s.conn
            .execute("DELETE FROM events WHERE seq = 2", [])
            .unwrap();
        assert_eq!(s.event_count().unwrap(), 2, "one fewer row");
        assert_eq!(
            s.head_seq().unwrap(),
            3,
            "the head does not move because a row went away"
        );
    }
    use crate::storage::test_helpers::run_conformance_suite;

    #[test]
    fn sqlite_passes_conformance_suite() {
        let mut storage = SqliteStorage::open(":memory:").unwrap();
        run_conformance_suite(&mut storage);
    }

    #[test]
    fn sqlite_persistence_across_reopen() {
        let path = format!("/tmp/gap-test-{}.db", crate::new_id("t"));
        {
            let mut s = SqliteStorage::open(&path).unwrap();
            s.append_event("a", serde_json::json!({ "x": 1 })).unwrap();
            s.append_event("b", serde_json::json!({ "x": 2 })).unwrap();
        }
        // Reopen: data survives, sequences are stable.
        {
            let s = SqliteStorage::open(&path).unwrap();
            assert_eq!(s.event_count().unwrap(), 2);
            // Sequences are 1-based, so after=1 -> the second event.
            let events = s.events_after(1, 10).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].kind, "b");
            assert_eq!(events[0].seq, 2);
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-shm"));
    }

    #[test]
    fn sqlite_sequence_continues_after_reopen() {
        // Regression for the O(n) MAX(seq) bug: the in-memory counter
        // must continue from the persisted max after a reopen.
        let path = format!("/tmp/gap-test-{}.db", crate::new_id("t"));
        {
            let mut s = SqliteStorage::open(&path).unwrap();
            s.append_event("a", serde_json::json!({})).unwrap();
        }
        {
            let mut s = SqliteStorage::open(&path).unwrap();
            let seq = s.append_event("b", serde_json::json!({})).unwrap();
            assert_eq!(seq, 2, "counter must resume after reopen");
            let seq = s.append_event("c", serde_json::json!({})).unwrap();
            assert_eq!(seq, 3);
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-shm"));
    }

    #[test]
    fn sqlite_invalid_events_rejected() {
        let mut s = SqliteStorage::open(":memory:").unwrap();
        assert!(s.append_event("", serde_json::json!({})).is_err());
        assert!(s.append_event("kind", serde_json::Value::Null).is_err());
        assert_eq!(s.event_count().unwrap(), 0);
    }
}
