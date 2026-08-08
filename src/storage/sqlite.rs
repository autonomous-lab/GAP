//! SQLite storage backend — local development and tests.
//!
//! One file, zero configuration, ACID. The state is materialized in
//! regular tables; the event spine is append-only with a monotonic
//! sequence. This is the backend used by the example and the test
//! suite.

use super::{AnnouncementRecord, ContractRecord, EventRecord, Storage, validate_event};
use crate::error::{Error, Result};
use rusqlite::{Connection, params};

/// SQLite-backed storage.
pub struct SqliteStorage {
    conn: Connection,
}

impl SqliteStorage {
    /// Open (or create) a database at `path`. Use `:memory:` for tests.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)
            .map_err(|e| Error::Other(format!("sqlite open failed: {e}")))?;
        let mut s = Self { conn };
        s.init()?;
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
                     payload TEXT NOT NULL
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
                 );",
            )
            .map_err(|e| Error::Other(format!("sqlite init failed: {e}")))?;
        Ok(())
    }
}

impl Storage for SqliteStorage {
    fn append_event(&mut self, kind: &str, payload: serde_json::Value) -> Result<u64> {
        validate_event(kind, &payload)?;
        let at = crate::message::now_unix() as i64;
        // Explicit 0-based sequence (not the rowid, which starts at 1).
        let seq = self.conn.query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM events",
            [],
            |r| r.get::<_, i64>(0),
        ).map_err(|e| Error::Other(format!("sqlite seq failed: {e}")))?;
        self.conn
            .execute(
                "INSERT INTO events (seq, kind, at, payload) VALUES (?1, ?2, ?3, ?4)",
                params![seq, kind, at, payload.to_string()],
            )
            .map_err(|e| Error::Other(format!("sqlite insert failed: {e}")))?;
        Ok(seq as u64)
    }

    fn events_after(&self, seq: u64, limit: u64) -> Result<Vec<EventRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, kind, at, payload FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2")
            .map_err(|e| Error::Other(format!("sqlite prepare failed: {e}")))?;
        let rows = stmt
            .query_map(params![seq as i64, limit as i64], |row| {
                Ok(EventRecord {
                    seq: row.get::<_, i64>(0)? as u64,
                    kind: row.get(1)?,
                    at: row.get::<_, i64>(2)? as u64,
                    payload: serde_json::from_str(&row.get::<_, String>(3)?)
                        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e)))?,
                })
            })
            .map_err(|e| Error::Other(format!("sqlite query failed: {e}")))?;
        let mut out = vec![];
        for row in rows {
            out.push(row.map_err(|e| Error::Other(format!("sqlite row failed: {e}")))?);
        }
        Ok(out)
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

    fn reap_expired(&mut self) -> Result<usize> {
        let now = crate::message::now_unix() as i64;
        let n = self
            .conn
            .execute("DELETE FROM announcements WHERE expires_at <= ?1", params![now])
            .map_err(|e| Error::Other(format!("sqlite reap failed: {e}")))?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            // Strictly-after seq 0 -> the second event survived.
            let events = s.events_after(0, 10).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].kind, "b");
            assert_eq!(events[0].seq, 1);
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
