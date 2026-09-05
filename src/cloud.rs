//! GAP Runtime project storage.
//!
//! Global project discovery and audit belong in GAP's ClickHouse-backed
//! event spine. Tenant data does not: every project gets its own SQLite
//! files so it can be snapshotted, moved and deleted without touching a
//! neighbour. This module is deliberately only storage; untrusted function
//! source is never executed here.

use crate::error::{Error, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MAX_KEY_BYTES: usize = 512;
pub const MAX_KV_BYTES: usize = 64 * 1024;
pub const MAX_OBJECT_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_FUNCTION_BYTES: usize = 512 * 1024;
pub const MAX_PROJECT_KV_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_PROJECT_OBJECT_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseRuling {
    Pending,
    Approved,
    ApprovedWithConstraints,
    NeedsReview,
    Rejected,
    Quarantined,
}

impl ReleaseRuling {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::ApprovedWithConstraints => "approved_with_constraints",
            Self::NeedsReview => "needs_review",
            Self::Rejected => "rejected",
            Self::Quarantined => "quarantined",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredObject {
    pub key: String,
    pub content: Vec<u8>,
    pub media_type: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionVersion {
    pub name: String,
    pub version: u64,
    pub runtime: String,
    pub source: Vec<u8>,
    pub digest: String,
    pub ruling: ReleaseRuling,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub project_id: String,
    pub owner_did: String,
    pub status: String,
    pub plan: String,
    pub created_at: u64,
    pub updated_at: u64,
}

pub struct ProjectStore {
    project_id: String,
    root: PathBuf,
    control: Connection,
}

impl ProjectStore {
    pub fn open(base: &Path, project_id: &str) -> Result<Self> {
        validate_identifier("project_id", project_id)?;
        let root = base.join(project_id);
        std::fs::create_dir_all(&root)
            .map_err(|e| Error::Other(format!("create project directory: {e}")))?;
        let control = Connection::open(root.join("control.sqlite"))
            .map_err(|e| Error::Other(format!("open project control database: {e}")))?;
        control
            .execute_batch(SCHEMA)
            .map_err(|e| Error::Other(format!("migrate project control database: {e}")))?;
        // Create the user database independently. It is intentionally not
        // attached to the control connection: tenant SQL must never reach
        // GAP metadata through an ATTACHed schema.
        Connection::open(root.join("database.sqlite"))
            .map_err(|e| Error::Other(format!("create project database: {e}")))?;
        Ok(Self {
            project_id: project_id.into(),
            root,
            control,
        })
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }
    pub fn user_database_path(&self) -> PathBuf {
        self.root.join("database.sqlite")
    }

    pub fn put_kv(
        &mut self,
        key: &str,
        value: &[u8],
        expires_at: Option<u64>,
        now: u64,
    ) -> Result<()> {
        validate_key(key)?;
        enforce_size("KV value", value.len(), MAX_KV_BYTES)?;
        let tx = self.control.transaction().map_err(db_error)?;
        enforce_project_quota(
            &tx,
            "kv",
            "key",
            "length(value)",
            key,
            value.len() as u64,
            MAX_PROJECT_KV_BYTES,
        )?;
        tx.execute(
            "INSERT INTO kv(key,value,expires_at,updated_at) VALUES(?1,?2,?3,?4) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value, expires_at=excluded.expires_at, updated_at=excluded.updated_at",
            params![key, value, expires_at.map(sql_int).transpose()?, sql_int(now)?],
        ).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(())
    }

    pub fn get_kv(&self, key: &str, now: u64) -> Result<Option<Vec<u8>>> {
        validate_key(key)?;
        self.control
            .query_row(
                "SELECT value FROM kv WHERE key=?1 AND (expires_at IS NULL OR expires_at>?2)",
                params![key, sql_int(now)?],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)
    }

    pub fn put_object(
        &mut self,
        key: &str,
        content: &[u8],
        media_type: &str,
        now: u64,
    ) -> Result<String> {
        validate_key(key)?;
        enforce_size("object", content.len(), MAX_OBJECT_BYTES)?;
        if media_type.len() > 255 || media_type.contains('\r') || media_type.contains('\n') {
            return Err(Error::Other("invalid object media_type".into()));
        }
        let digest = format!("sha256:{}", crate::sha256_hex(content));
        let tx = self.control.transaction().map_err(db_error)?;
        enforce_project_quota(
            &tx,
            "objects",
            "object_key",
            "size_bytes",
            key,
            content.len() as u64,
            MAX_PROJECT_OBJECT_BYTES,
        )?;
        tx.execute(
            "INSERT INTO objects(object_key,content,media_type,size_bytes,digest,created_at) VALUES(?1,?2,?3,?4,?5,?6) \
             ON CONFLICT(object_key) DO UPDATE SET content=excluded.content,media_type=excluded.media_type,size_bytes=excluded.size_bytes,digest=excluded.digest,created_at=excluded.created_at",
            params![key, content, media_type, sql_int(content.len() as u64)?, digest, sql_int(now)?],
        ).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(digest)
    }

    pub fn get_object(&self, key: &str) -> Result<Option<StoredObject>> {
        validate_key(key)?;
        self.control
            .query_row(
                "SELECT content,media_type,digest FROM objects WHERE object_key=?1",
                params![key],
                |r| {
                    Ok(StoredObject {
                        key: key.into(),
                        content: r.get(0)?,
                        media_type: r.get(1)?,
                        digest: r.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(db_error)
    }

    pub fn deploy_function(
        &mut self,
        name: &str,
        runtime: &str,
        source: &[u8],
        now: u64,
    ) -> Result<FunctionVersion> {
        validate_identifier("function name", name)?;
        if !matches!(runtime, "javascript" | "wasm") {
            return Err(Error::Other("runtime must be javascript or wasm".into()));
        }
        enforce_size("function", source.len(), MAX_FUNCTION_BYTES)?;
        let digest = format!("sha256:{}", crate::sha256_hex(source));
        let tx = self.control.transaction().map_err(db_error)?;
        let version: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(version),0)+1 FROM function_versions WHERE name=?1",
                [name],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        tx.execute(
            "INSERT OR IGNORE INTO functions(name,created_at) VALUES(?1,?2)",
            params![name, sql_int(now)?],
        )
        .map_err(db_error)?;
        tx.execute(
            "INSERT INTO function_versions(name,version,runtime,source,digest,ruling,created_at) VALUES(?1,?2,?3,?4,?5,'pending',?6)",
            params![name, version, runtime, source, digest, sql_int(now)?],
        ).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(FunctionVersion {
            name: name.into(),
            version: version as u64,
            runtime: runtime.into(),
            source: source.into(),
            digest,
            ruling: ReleaseRuling::Pending,
            active: false,
        })
    }

    pub fn set_function_ruling(
        &mut self,
        name: &str,
        version: u64,
        ruling: ReleaseRuling,
    ) -> Result<()> {
        validate_identifier("function name", name)?;
        let changed = self
            .control
            .execute(
                "UPDATE function_versions SET ruling=?3 WHERE name=?1 AND version=?2",
                params![name, sql_int(version)?, ruling.as_str()],
            )
            .map_err(db_error)?;
        if changed == 0 {
            return Err(Error::Other("unknown function version".into()));
        }
        Ok(())
    }

    pub fn activate_function(&mut self, name: &str, version: u64) -> Result<()> {
        validate_identifier("function name", name)?;
        let ruling: Option<String> = self
            .control
            .query_row(
                "SELECT ruling FROM function_versions WHERE name=?1 AND version=?2",
                params![name, sql_int(version)?],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let allowed = matches!(
            ruling.as_deref(),
            Some("approved" | "approved_with_constraints")
        );
        if !allowed {
            return Err(Error::Other(
                "function version has no activating release verdict".into(),
            ));
        }
        self.control
            .execute(
                "UPDATE functions SET active_version=?2 WHERE name=?1",
                params![name, sql_int(version)?],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn active_function(&self, name: &str) -> Result<Option<FunctionVersion>> {
        validate_identifier("function name", name)?;
        self.control
            .query_row(
                "SELECT v.version,v.runtime,v.source,v.digest,v.ruling \
                 FROM functions f JOIN function_versions v \
                 ON v.name=f.name AND v.version=f.active_version WHERE f.name=?1",
                [name],
                |r| {
                    let ruling: String = r.get(4)?;
                    Ok(FunctionVersion {
                        name: name.into(),
                        version: r.get::<_, i64>(0)? as u64,
                        runtime: r.get(1)?,
                        source: r.get(2)?,
                        digest: r.get(3)?,
                        ruling: parse_ruling(&ruling),
                        active: true,
                    })
                },
            )
            .optional()
            .map_err(db_error)
    }
}

fn enforce_project_quota(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    key_column: &str,
    size_expression: &str,
    key: &str,
    incoming_bytes: u64,
    limit: u64,
) -> Result<()> {
    // Identifiers are fixed call-site constants, never request data.
    let sql = format!(
        "SELECT COALESCE(SUM({size_expression}),0), \
         COALESCE(MAX(CASE WHEN {key_column}=?1 THEN {size_expression} ELSE 0 END),0) \
         FROM {table}"
    );
    let (used, replaced): (i64, i64) = tx
        .query_row(&sql, [key], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(db_error)?;
    let projected = (used.max(0) as u64)
        .saturating_sub(replaced.max(0) as u64)
        .saturating_add(incoming_bytes);
    if projected > limit {
        return Err(Error::Other(format!(
            "project {table} quota exceeded: {projected} bytes would exceed {limit}"
        )));
    }
    Ok(())
}

fn parse_ruling(value: &str) -> ReleaseRuling {
    match value {
        "approved" => ReleaseRuling::Approved,
        "approved_with_constraints" => ReleaseRuling::ApprovedWithConstraints,
        "needs_review" => ReleaseRuling::NeedsReview,
        "rejected" => ReleaseRuling::Rejected,
        "quarantined" => ReleaseRuling::Quarantined,
        _ => ReleaseRuling::Pending,
    }
}

fn validate_identifier(field: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 80
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(Error::Other(format!("invalid {field}")));
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES || key.contains('\0') {
        return Err(Error::Other("invalid or oversized key".into()));
    }
    Ok(())
}

fn enforce_size(kind: &str, actual: usize, limit: usize) -> Result<()> {
    if actual > limit {
        Err(Error::Other(format!(
            "{kind} is {actual} bytes, limit is {limit}"
        )))
    } else {
        Ok(())
    }
}

fn db_error(e: rusqlite::Error) -> Error {
    Error::Other(format!("project database: {e}"))
}

fn sql_int(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Other("integer exceeds SQLite range".into()))
}

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS kv(key TEXT PRIMARY KEY,value BLOB NOT NULL,expires_at INTEGER,updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS objects(object_key TEXT PRIMARY KEY,content BLOB NOT NULL,media_type TEXT NOT NULL,size_bytes INTEGER NOT NULL,digest TEXT NOT NULL,created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS functions(name TEXT PRIMARY KEY,active_version INTEGER,created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS function_versions(name TEXT NOT NULL,version INTEGER NOT NULL,runtime TEXT NOT NULL,source BLOB NOT NULL,digest TEXT NOT NULL,ruling TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(name,version),FOREIGN KEY(name) REFERENCES functions(name));
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_store() -> ProjectStore {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        ProjectStore::open(
            &std::env::temp_dir().join(format!("gap-cloud-{nonce}")),
            "prj_test",
        )
        .unwrap()
    }

    #[test]
    fn project_ids_cannot_escape_the_base_directory() {
        assert!(ProjectStore::open(&std::env::temp_dir(), "../escape").is_err());
        assert!(ProjectStore::open(&std::env::temp_dir(), "a/b").is_err());
    }

    #[test]
    fn kv_expires_and_enforces_limits() {
        let mut s = temp_store();
        s.put_kv("counter", b"1", Some(20), 10).unwrap();
        assert_eq!(s.get_kv("counter", 19).unwrap(), Some(b"1".to_vec()));
        assert_eq!(s.get_kv("counter", 20).unwrap(), None);
        assert!(s
            .put_kv("huge", &vec![0; MAX_KV_BYTES + 1], None, 10)
            .is_err());
    }

    #[test]
    fn objects_are_content_addressed() {
        let mut s = temp_store();
        let digest = s
            .put_object("report.json", br#"{"ok":true}"#, "application/json", 10)
            .unwrap();
        let object = s.get_object("report.json").unwrap().unwrap();
        assert_eq!(object.digest, digest);
        assert_eq!(object.content, br#"{"ok":true}"#);
    }

    #[test]
    fn project_quota_counts_replacements_without_double_charging() {
        let mut s = temp_store();
        s.put_kv("a", b"123", None, 1).unwrap();
        let tx = s.control.transaction().unwrap();
        assert!(enforce_project_quota(&tx, "kv", "key", "length(value)", "b", 2, 4).is_err());
        assert!(enforce_project_quota(&tx, "kv", "key", "length(value)", "a", 4, 4).is_ok());
    }

    #[test]
    fn function_cannot_activate_without_release_verdict() {
        let mut s = temp_store();
        let v1 = s
            .deploy_function("answer", "javascript", b"return 42", 10)
            .unwrap();
        assert_eq!(v1.version, 1);
        assert!(s.activate_function("answer", 1).is_err());
        s.set_function_ruling("answer", 1, ReleaseRuling::Approved)
            .unwrap();
        s.activate_function("answer", 1).unwrap();
        let v2 = s
            .deploy_function("answer", "javascript", b"return 43", 11)
            .unwrap();
        assert_eq!(v2.version, 2);
        assert!(s.activate_function("answer", 2).is_err());
    }

    #[test]
    fn oversized_and_unknown_runtimes_are_rejected() {
        let mut s = temp_store();
        assert!(s.deploy_function("x", "node", b"x", 1).is_err());
        assert!(s
            .deploy_function("x", "wasm", &vec![0; MAX_FUNCTION_BYTES + 1], 1)
            .is_err());
    }

    #[test]
    fn active_function_returns_exact_approved_version() {
        let mut s = temp_store();
        s.deploy_function("answer", "javascript", b"() => 42", 10)
            .unwrap();
        s.set_function_ruling("answer", 1, ReleaseRuling::ApprovedWithConstraints)
            .unwrap();
        s.activate_function("answer", 1).unwrap();
        let active = s.active_function("answer").unwrap().unwrap();
        assert_eq!(active.version, 1);
        assert_eq!(active.source, b"() => 42");
        assert_eq!(active.ruling, ReleaseRuling::ApprovedWithConstraints);
    }
}
