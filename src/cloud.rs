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
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const MAX_KEY_BYTES: usize = 512;
pub const MAX_KV_BYTES: usize = 64 * 1024;
pub const MAX_OBJECT_BYTES: usize = 1024 * 1024;
pub const MAX_FUNCTION_BYTES: usize = 1024 * 1024;
pub const MAX_PROJECT_KV_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_PROJECT_OBJECT_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_PROJECT_FUNCTION_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_PROJECT_DATABASE_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_DATABASE_SQL_BYTES: usize = 32 * 1024;
pub const MAX_DATABASE_PARAMS: usize = 100;
pub const MAX_DATABASE_ROWS: usize = 100;
pub const MAX_DATABASE_COLUMNS: usize = 50;
pub const MAX_DATABASE_CELL_BYTES: usize = 1024 * 1024;
pub const MAX_DATABASE_RESULT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DATABASE_TIME: Duration = Duration::from_millis(250);
pub const FUNCTION_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_FUNCTION_HTTP_RESPONSE_BYTES: usize = 3 * 1024 * 1024;
pub const MAX_SITE_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_PROJECT_SITE_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_SITE_FILES: u64 = 5_000;
pub const MAX_SITE_VERSIONS: u64 = 5;
pub const MAX_SITE_REQUESTS_PER_SECOND: u64 = 20;
pub const MAX_SITE_CUSTOM_DOMAINS: usize = 3;
pub const MAX_SITE_BANDWIDTH_PER_PERIOD: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteConfig {
    pub enabled: bool,
    pub entrypoint: String,
    pub spa_fallback: bool,
    pub username: String,
    pub active_version: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteVersion {
    pub version: u64,
    pub active: bool,
    pub file_count: u64,
    pub size_bytes: u64,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteAsset {
    pub content: Vec<u8>,
    pub media_type: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteFileInfo {
    pub path: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub digest: String,
}

/// Deterministic publication gate for JavaScript functions. This deliberately
/// targets intent and evasion indicators rather than pretending to be a full
/// JavaScript parser; runtime isolation remains authoritative.
pub fn scan_javascript_function(source: &[u8]) -> Result<Vec<String>> {
    let text = std::str::from_utf8(source)
        .map_err(|_| Error::Other("function source must be valid UTF-8".into()))?;
    if text
        .bytes()
        .any(|byte| byte == 0 || (byte < 0x09) || (byte > 0x0d && byte < 0x20))
    {
        return Err(Error::Other(
            "function security scan: control bytes are forbidden".into(),
        ));
    }
    if text.len() > 32 * 1024 && text.lines().any(|line| line.len() > 32 * 1024) {
        return Err(Error::Other(
            "function security scan: excessive minification or padding".into(),
        ));
    }

    let lower = text.to_ascii_lowercase();
    let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    let forbidden = [
        ("process.env", "environment-secret access"),
        ("deno.env", "environment-secret access"),
        ("bun.env", "environment-secret access"),
        ("child_process", "process execution"),
        ("worker_threads", "worker creation"),
        ("require(", "module loading"),
        ("import(", "dynamic module loading"),
        ("eval(", "dynamic code execution"),
        ("newfunction(", "dynamic code execution"),
        ("__proto__", "prototype manipulation"),
        ("constructor.prototype", "prototype manipulation"),
        ("xmlhttprequest", "unbrokered network access"),
        ("websocket(", "unbrokered network access"),
        ("fetch(", "unbrokered network access; use gap.http"),
    ];
    for (pattern, reason) in forbidden {
        if compact.contains(pattern) {
            return Err(Error::Other(format!(
                "function security scan: forbidden {reason}"
            )));
        }
    }

    let encoded_markers = compact.matches("\\x").count()
        + compact.matches("\\u00").count()
        + compact.matches("fromcharcode").count()
        + compact.matches("atob(").count()
        + compact.matches("unescape(").count();
    if encoded_markers > 32 {
        return Err(Error::Other(
            "function security scan: excessive encoded or obfuscated content".into(),
        ));
    }
    let has_http = compact.contains("gap.http.");
    let has_loop = ["for(", "while(", "dowhile("]
        .iter()
        .any(|p| compact.contains(p));
    if has_http && (has_loop || compact.contains("promise.all(")) {
        return Err(Error::Other(
            "function security scan: bulk or looped outbound HTTP is forbidden".into(),
        ));
    }

    let mut findings = Vec::new();
    if has_http {
        findings.push("outbound HTTP requires semantic review and the project allowlist".into());
    }
    if compact.contains("gap.kv.")
        || compact.contains("gap.db.")
        || compact.contains("gap.objects.")
    {
        findings.push("project data access requires semantic exfiltration review".into());
    }
    if compact.contains("gap.realtime.") {
        findings.push("realtime token issuance requires semantic scope review".into());
    }
    Ok(findings)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatabaseResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub affected_rows: usize,
    pub truncated: bool,
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_review: Option<FunctionSecurityReview>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSecurityReview {
    pub judge: String,
    pub static_findings: Vec<String>,
    pub reasons: Vec<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteDomain {
    pub hostname: String,
    pub project_id: String,
    /// `public` or `basic`. The GAP-owned `/sites/{project}/` URL always
    /// remains Basic-authenticated regardless of this value.
    pub access: String,
    /// `pending_dns`, `active` or `suspended`.
    pub status: String,
    pub verification_name: String,
    pub verification_value: String,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionHttpPolicy {
    pub auth: String,
    pub cors_origins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionSchedule {
    pub id: String,
    pub function: String,
    pub cron: String,
    pub request: Value,
    pub enabled: bool,
    pub next_run_at: u64,
    pub last_run_at: Option<u64>,
    pub last_status: Option<String>,
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

    pub fn database_query(&self, sql: &str, params: &[Value]) -> Result<DatabaseResult> {
        self.database_statement(sql, params, true)
    }

    pub fn database_execute(&self, sql: &str, params: &[Value]) -> Result<DatabaseResult> {
        self.database_statement(sql, params, false)
    }

    fn database_statement(
        &self,
        sql: &str,
        values: &[Value],
        query: bool,
    ) -> Result<DatabaseResult> {
        if sql.trim().is_empty() || sql.len() > MAX_DATABASE_SQL_BYTES {
            return Err(Error::Other("database SQL is empty or too large".into()));
        }
        if values.len() > MAX_DATABASE_PARAMS {
            return Err(Error::Other("too many database parameters".into()));
        }
        let params = values
            .iter()
            .map(json_to_sql_value)
            .collect::<Result<Vec<_>>>()?;
        let connection = Connection::open(self.user_database_path()).map_err(db_error)?;
        connection
            .busy_timeout(Duration::from_millis(100))
            .map_err(db_error)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(db_error)?;
        let page_size: i64 = connection
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .map_err(db_error)?;
        let max_pages = sql_int(MAX_PROJECT_DATABASE_BYTES / (page_size.max(1) as u64))?;
        connection
            .pragma_update(None, "max_page_count", max_pages)
            .map_err(db_error)?;
        install_database_guards(&connection)?;

        let mut statement = connection.prepare(sql).map_err(db_error)?;
        if statement.column_count() > MAX_DATABASE_COLUMNS {
            return Err(Error::Other("database result has too many columns".into()));
        }
        if query {
            let columns = statement
                .column_names()
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>();
            let mut cursor = statement
                .query(rusqlite::params_from_iter(params.iter()))
                .map_err(db_error)?;
            let mut rows = Vec::new();
            let mut truncated = false;
            let mut result_bytes = 0usize;
            while let Some(row) = cursor.next().map_err(db_error)? {
                if rows.len() == MAX_DATABASE_ROWS {
                    truncated = true;
                    break;
                }
                let mut output = Vec::with_capacity(columns.len());
                for index in 0..columns.len() {
                    let (value, bytes) = sql_to_json(row.get_ref(index).map_err(db_error)?)?;
                    result_bytes = result_bytes.saturating_add(bytes);
                    if result_bytes > MAX_DATABASE_RESULT_BYTES {
                        return Err(Error::Other("database result is too large".into()));
                    }
                    output.push(value);
                }
                rows.push(output);
            }
            Ok(DatabaseResult {
                columns,
                rows,
                affected_rows: 0,
                truncated,
            })
        } else {
            if statement.column_count() != 0 {
                return Err(Error::Other(
                    "use database query mode for returned rows".into(),
                ));
            }
            let affected_rows = statement
                .execute(rusqlite::params_from_iter(params.iter()))
                .map_err(db_error)?;
            Ok(DatabaseResult {
                columns: Vec::new(),
                rows: Vec::new(),
                affected_rows,
                truncated: false,
            })
        }
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

    pub fn configure_site(
        &mut self,
        enabled: bool,
        entrypoint: &str,
        spa_fallback: bool,
        username: &str,
        password: Option<&str>,
        now: u64,
    ) -> Result<SiteConfig> {
        validate_site_path(entrypoint)?;
        if username.is_empty()
            || username.len() > 64
            || !username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@'))
        {
            return Err(Error::Other("invalid site Basic Auth username".into()));
        }
        let existing_hash: Option<String> = self
            .control
            .query_row(
                "SELECT password_hash FROM site_config WHERE id=1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let password_hash = match password {
            Some(password) => hash_site_password(password)?,
            None => existing_hash.ok_or_else(|| {
                Error::Other(
                    "password is required when configuring a site for the first time".into(),
                )
            })?,
        };
        self.control.execute(
            "INSERT INTO site_config(id,enabled,entrypoint,spa_fallback,username,password_hash,updated_at) \
             VALUES(1,?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET \
             enabled=excluded.enabled,entrypoint=excluded.entrypoint,spa_fallback=excluded.spa_fallback,\
             username=excluded.username,password_hash=excluded.password_hash,updated_at=excluded.updated_at",
            params![i64::from(enabled), entrypoint, i64::from(spa_fallback), username, password_hash, sql_int(now)?],
        ).map_err(db_error)?;
        self.site_config()?
            .ok_or_else(|| Error::Other("site configuration was not stored".into()))
    }

    pub fn site_config(&self) -> Result<Option<SiteConfig>> {
        self.control.query_row(
            "SELECT enabled,entrypoint,spa_fallback,username,active_version FROM site_config WHERE id=1",
            [],
            |r| Ok(SiteConfig {
                enabled: r.get::<_, i64>(0)? != 0,
                entrypoint: r.get(1)?,
                spa_fallback: r.get::<_, i64>(2)? != 0,
                username: r.get(3)?,
                active_version: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
            }),
        ).optional().map_err(db_error)
    }

    pub fn create_site_version(&mut self, now: u64) -> Result<SiteVersion> {
        if self.site_config()?.is_none() {
            return Err(Error::Other(
                "configure the site before creating a version".into(),
            ));
        }
        let count: i64 = self
            .control
            .query_row("SELECT COUNT(*) FROM site_versions", [], |r| r.get(0))
            .map_err(db_error)?;
        if count.max(0) as u64 >= MAX_SITE_VERSIONS {
            return Err(Error::Other(format!(
                "site version limit is {MAX_SITE_VERSIONS}; delete an inactive version first"
            )));
        }
        let version: i64 = self
            .control
            .query_row(
                "SELECT COALESCE(MAX(version),0)+1 FROM site_versions",
                [],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        self.control
            .execute(
                "INSERT INTO site_versions(version,created_at) VALUES(?1,?2)",
                params![version, sql_int(now)?],
            )
            .map_err(db_error)?;
        self.site_version(version as u64)?
            .ok_or_else(|| Error::Other("site version was not stored".into()))
    }

    pub fn site_versions(&self) -> Result<Vec<SiteVersion>> {
        let mut statement = self
            .control
            .prepare(
                "SELECT v.version,CASE WHEN c.active_version=v.version THEN 1 ELSE 0 END,\
             COUNT(f.path),COALESCE(SUM(f.size_bytes),0),v.created_at FROM site_versions v \
             LEFT JOIN site_config c ON c.id=1 LEFT JOIN site_files f ON f.version=v.version \
             GROUP BY v.version,c.active_version,v.created_at ORDER BY v.version DESC",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map([], site_version_row)
            .map_err(db_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_error)
    }

    pub fn site_version(&self, version: u64) -> Result<Option<SiteVersion>> {
        self.site_versions()
            .map(|versions| versions.into_iter().find(|v| v.version == version))
    }

    pub fn put_site_file(
        &mut self,
        version: u64,
        path: &str,
        content: &[u8],
        now: u64,
    ) -> Result<SiteAsset> {
        validate_site_path(path)?;
        enforce_size("site file", content.len(), MAX_SITE_FILE_BYTES)?;
        scan_static_site_file(path, content)?;
        let media_type = site_media_type(path)?.to_string();
        let digest = format!("sha256:{}", crate::sha256_hex(content));
        let tx = self.control.transaction().map_err(db_error)?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT version FROM site_versions WHERE version=?1",
                [sql_int(version)?],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        if exists.is_none() {
            return Err(Error::Other("unknown site version".into()));
        }
        let active: Option<i64> = tx
            .query_row(
                "SELECT active_version FROM site_config WHERE id=1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?
            .flatten();
        if active == Some(sql_int(version)?) {
            return Err(Error::Other("an active site version is immutable".into()));
        }
        let file_count: i64 = tx
            .query_row("SELECT COUNT(*) FROM site_files", [], |r| r.get(0))
            .map_err(db_error)?;
        let replacing: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM site_files WHERE version=?1 AND path=?2",
                params![sql_int(version)?, path],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        if replacing == 0 && file_count.max(0) as u64 >= MAX_SITE_FILES {
            return Err(Error::Other(format!("site file limit is {MAX_SITE_FILES}")));
        }
        let used: i64 = tx
            .query_row(
                "SELECT COALESCE(SUM(size_bytes),0) FROM site_files",
                [],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        let replaced: i64 = tx
            .query_row(
                "SELECT COALESCE(size_bytes,0) FROM site_files WHERE version=?1 AND path=?2",
                params![sql_int(version)?, path],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?
            .unwrap_or(0);
        let projected = (used.max(0) as u64)
            .saturating_sub(replaced.max(0) as u64)
            .saturating_add(content.len() as u64);
        if projected > MAX_PROJECT_SITE_BYTES {
            return Err(Error::Other(format!("project site quota exceeded: {projected} bytes would exceed {MAX_PROJECT_SITE_BYTES}")));
        }
        tx.execute(
            "INSERT INTO site_files(version,path,content,media_type,size_bytes,digest,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7) \
             ON CONFLICT(version,path) DO UPDATE SET content=excluded.content,media_type=excluded.media_type,size_bytes=excluded.size_bytes,digest=excluded.digest,created_at=excluded.created_at",
            params![sql_int(version)?, path, content, media_type, sql_int(content.len() as u64)?, digest, sql_int(now)?],
        ).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(SiteAsset {
            content: content.to_vec(),
            media_type,
            digest,
        })
    }

    pub fn site_files(&self, version: u64) -> Result<Vec<SiteFileInfo>> {
        let mut statement = self.control.prepare(
            "SELECT path,media_type,size_bytes,digest FROM site_files WHERE version=?1 ORDER BY path"
        ).map_err(db_error)?;
        let rows = statement
            .query_map([sql_int(version)?], |row| {
                Ok(SiteFileInfo {
                    path: row.get(0)?,
                    media_type: row.get(1)?,
                    size_bytes: row.get::<_, i64>(2)? as u64,
                    digest: row.get(3)?,
                })
            })
            .map_err(db_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_error)
    }

    pub fn delete_site_file(&mut self, version: u64, path: &str) -> Result<bool> {
        validate_site_path(path)?;
        if self.site_config()?.and_then(|config| config.active_version) == Some(version) {
            return Err(Error::Other("an active site version is immutable".into()));
        }
        Ok(self
            .control
            .execute(
                "DELETE FROM site_files WHERE version=?1 AND path=?2",
                params![sql_int(version)?, path],
            )
            .map_err(db_error)?
            > 0)
    }

    pub fn activate_site_version(&mut self, version: u64) -> Result<SiteConfig> {
        let config = self
            .site_config()?
            .ok_or_else(|| Error::Other("site is not configured".into()))?;
        let entrypoint: Option<i64> = self
            .control
            .query_row(
                "SELECT 1 FROM site_files WHERE version=?1 AND path=?2",
                params![sql_int(version)?, config.entrypoint],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        if entrypoint.is_none() {
            return Err(Error::Other(format!(
                "site version has no {} entrypoint",
                config.entrypoint
            )));
        }
        self.control
            .execute(
                "UPDATE site_config SET active_version=?1 WHERE id=1",
                [sql_int(version)?],
            )
            .map_err(db_error)?;
        self.site_config()?
            .ok_or_else(|| Error::Other("site configuration disappeared".into()))
    }

    pub fn delete_site_version(&mut self, version: u64) -> Result<bool> {
        let active = self.site_config()?.and_then(|c| c.active_version);
        if active == Some(version) {
            return Err(Error::Other("cannot delete the active site version".into()));
        }
        Ok(self
            .control
            .execute(
                "DELETE FROM site_versions WHERE version=?1",
                [sql_int(version)?],
            )
            .map_err(db_error)?
            > 0)
    }

    pub fn authenticate_site(&self, username: &str, password: &str) -> Result<bool> {
        let row: Option<(String, String, i64)> = self
            .control
            .query_row(
                "SELECT username,password_hash,enabled FROM site_config WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(db_error)?;
        let Some((expected_username, hash, enabled)) = row else {
            return Ok(false);
        };
        if enabled == 0 || username != expected_username {
            return Ok(false);
        }
        verify_site_password(password, &hash)
    }

    pub fn active_site_asset(&self, requested_path: &str) -> Result<Option<SiteAsset>> {
        let config = self
            .site_config()?
            .ok_or_else(|| Error::Other("site is not configured".into()))?;
        if !config.enabled {
            return Ok(None);
        }
        let version = config
            .active_version
            .ok_or_else(|| Error::Other("site has no active version".into()))?;
        let path = if requested_path.is_empty() {
            config.entrypoint.clone()
        } else {
            requested_path.to_string()
        };
        validate_site_path(&path)?;
        let load =
            |path: &str| -> Result<Option<SiteAsset>> {
                self.control.query_row(
                "SELECT content,media_type,digest FROM site_files WHERE version=?1 AND path=?2",
                params![sql_int(version)?, path],
                |r| Ok(SiteAsset { content: r.get(0)?, media_type: r.get(1)?, digest: r.get(2)? }),
            ).optional().map_err(db_error)
            };
        match load(&path)? {
            Some(asset) => Ok(Some(asset)),
            None if config.spa_fallback && !path.contains('.') => load(&config.entrypoint),
            None => Ok(None),
        }
    }

    pub fn consume_site_quota(&mut self, bytes: u64, now: u64) -> Result<()> {
        let second = sql_int(now)?;
        let period = sql_int(now / (30 * 24 * 60 * 60))?;
        let tx = self.control.transaction().map_err(db_error)?;
        tx.execute(
            "DELETE FROM site_rate WHERE second<?1",
            [second.saturating_sub(2)],
        )
        .map_err(db_error)?;
        let requests: i64 = tx
            .query_row(
                "SELECT requests FROM site_rate WHERE second=?1",
                [second],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?
            .unwrap_or(0);
        if requests.max(0) as u64 >= MAX_SITE_REQUESTS_PER_SECOND {
            return Err(Error::Other("site request rate exceeded".into()));
        }
        let used: i64 = tx
            .query_row(
                "SELECT bytes FROM site_bandwidth WHERE period=?1",
                [period],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?
            .unwrap_or(0);
        if (used.max(0) as u64).saturating_add(bytes) > MAX_SITE_BANDWIDTH_PER_PERIOD {
            return Err(Error::Other("site bandwidth quota exceeded".into()));
        }
        tx.execute("INSERT INTO site_rate(second,requests) VALUES(?1,1) ON CONFLICT(second) DO UPDATE SET requests=requests+1", [second]).map_err(db_error)?;
        tx.execute("INSERT INTO site_bandwidth(period,bytes) VALUES(?1,?2) ON CONFLICT(period) DO UPDATE SET bytes=bytes+excluded.bytes", params![period, sql_int(bytes)?]).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }

    pub fn set_egress_hosts(&mut self, hosts: &[String]) -> Result<()> {
        if hosts.len() > 32 {
            return Err(Error::Other("too many egress hosts".into()));
        }
        let mut normalized = Vec::new();
        for host in hosts {
            let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
            if host.is_empty() || host.len() > 253 || host.contains(['/', ':', '@', '?', '#']) {
                return Err(Error::Other("invalid egress host".into()));
            }
            normalized.push(host);
        }
        normalized.sort();
        normalized.dedup();
        let tx = self.control.transaction().map_err(db_error)?;
        tx.execute("DELETE FROM egress_hosts", [])
            .map_err(db_error)?;
        for host in normalized {
            tx.execute("INSERT INTO egress_hosts(host) VALUES(?1)", [host])
                .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)
    }

    pub fn egress_hosts(&self) -> Result<Vec<String>> {
        let mut statement = self
            .control
            .prepare("SELECT host FROM egress_hosts ORDER BY host")
            .map_err(db_error)?;
        let rows = statement
            .query_map([], |row| row.get(0))
            .map_err(db_error)?;
        rows.collect::<std::result::Result<Vec<String>, _>>()
            .map_err(db_error)
    }

    pub fn set_function_http_policy(
        &mut self,
        name: &str,
        auth: &str,
        origins: &[String],
    ) -> Result<()> {
        validate_identifier("function name", name)?;
        if !matches!(auth, "private" | "token" | "public")
            || origins.len() > 16
            || origins
                .iter()
                .any(|o| o.len() > 255 || o.contains(['\r', '\n']))
        {
            return Err(Error::Other("invalid function HTTP policy".into()));
        }
        let cors = serde_json::to_string(origins).map_err(|e| Error::Other(e.to_string()))?;
        self.control.execute(
            "INSERT INTO function_http(name,auth,cors_origins) VALUES(?1,?2,?3) ON CONFLICT(name) DO UPDATE SET auth=excluded.auth,cors_origins=excluded.cors_origins",
            params![name, auth, cors],
        ).map_err(db_error)?;
        Ok(())
    }

    pub fn function_http_policy(&self, name: &str) -> Result<FunctionHttpPolicy> {
        validate_identifier("function name", name)?;
        let row: Option<(String, String)> = self
            .control
            .query_row(
                "SELECT auth,cors_origins FROM function_http WHERE name=?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_error)?;
        let (auth, cors) = row.unwrap_or_else(|| ("private".into(), "[]".into()));
        Ok(FunctionHttpPolicy {
            auth,
            cors_origins: serde_json::from_str(&cors).unwrap_or_default(),
        })
    }

    pub fn put_schedule(
        &mut self,
        id: &str,
        function: &str,
        cron: &str,
        request: &Value,
        now: u64,
    ) -> Result<FunctionSchedule> {
        validate_identifier("schedule id", id)?;
        validate_identifier("function name", function)?;
        let minutes = parse_minute_cron(cron)?;
        let next = now.saturating_add(minutes.saturating_mul(60));
        let request = serde_json::to_string(request).map_err(|e| Error::Other(e.to_string()))?;
        if request.len() > 64 * 1024 {
            return Err(Error::Other("schedule request is too large".into()));
        }
        self.control.execute(
            "INSERT INTO schedules(id,function,cron,request,enabled,next_run_at) VALUES(?1,?2,?3,?4,1,?5) ON CONFLICT(id) DO UPDATE SET function=excluded.function,cron=excluded.cron,request=excluded.request,enabled=1,next_run_at=excluded.next_run_at",
            params![id, function, cron, request, sql_int(next)?],
        ).map_err(db_error)?;
        self.schedule(id)?
            .ok_or_else(|| Error::Other("schedule was not stored".into()))
    }

    pub fn schedules(&self) -> Result<Vec<FunctionSchedule>> {
        let mut stmt = self.control.prepare("SELECT id,function,cron,request,enabled,next_run_at,last_run_at,last_status FROM schedules ORDER BY id").map_err(db_error)?;
        let rows = stmt.query_map([], schedule_row).map_err(db_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_error)
    }

    pub fn schedule(&self, id: &str) -> Result<Option<FunctionSchedule>> {
        self.control.query_row("SELECT id,function,cron,request,enabled,next_run_at,last_run_at,last_status FROM schedules WHERE id=?1", [id], schedule_row).optional().map_err(db_error)
    }

    pub fn due_schedules(&self, now: u64) -> Result<Vec<FunctionSchedule>> {
        Ok(self
            .schedules()?
            .into_iter()
            .filter(|s| s.enabled && s.next_run_at <= now)
            .collect())
    }

    pub fn finish_schedule(&mut self, id: &str, now: u64, status: &str) -> Result<()> {
        let schedule = self
            .schedule(id)?
            .ok_or_else(|| Error::Other("unknown schedule".into()))?;
        let minutes = parse_minute_cron(&schedule.cron)?;
        self.control
            .execute(
                "UPDATE schedules SET last_run_at=?2,last_status=?3,next_run_at=?4 WHERE id=?1",
                params![
                    id,
                    sql_int(now)?,
                    status,
                    sql_int(now.saturating_add(minutes * 60))?
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn delete_schedule(&mut self, id: &str) -> Result<bool> {
        validate_identifier("schedule id", id)?;
        Ok(self
            .control
            .execute("DELETE FROM schedules WHERE id=?1", [id])
            .map_err(db_error)?
            > 0)
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
        enforce_additive_quota(
            &tx,
            "function_versions",
            "length(source)",
            source.len() as u64,
            MAX_PROJECT_FUNCTION_BYTES,
        )?;
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
            security_review: None,
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
                        security_review: None,
                    })
                },
            )
            .optional()
            .map_err(db_error)
    }

    /// Delete a function and all of its immutable versions. Returns false when
    /// the name did not exist, making the management endpoint idempotent.
    pub fn delete_function(&mut self, name: &str) -> Result<bool> {
        validate_identifier("function name", name)?;
        let tx = self.control.transaction().map_err(db_error)?;
        tx.execute("DELETE FROM function_versions WHERE name=?1", [name])
            .map_err(db_error)?;
        let deleted = tx
            .execute("DELETE FROM functions WHERE name=?1", [name])
            .map_err(db_error)?
            > 0;
        tx.commit().map_err(db_error)?;
        Ok(deleted)
    }

    /// Delete one inactive version. The active version is protected so a
    /// cleanup request cannot silently take a production function offline.
    pub fn delete_function_version(&mut self, name: &str, version: u64) -> Result<bool> {
        validate_identifier("function name", name)?;
        let version = sql_int(version)?;
        let tx = self.control.transaction().map_err(db_error)?;
        let active: Option<i64> = tx
            .query_row(
                "SELECT active_version FROM functions WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?
            .flatten();
        if active == Some(version) {
            return Err(Error::Other(
                "cannot delete the active function version".into(),
            ));
        }
        let deleted = tx
            .execute(
                "DELETE FROM function_versions WHERE name=?1 AND version=?2",
                params![name, version],
            )
            .map_err(db_error)?
            > 0;
        if deleted {
            tx.execute(
                "DELETE FROM functions WHERE name=?1 AND NOT EXISTS \
                 (SELECT 1 FROM function_versions WHERE name=?1)",
                [name],
            )
            .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)?;
        Ok(deleted)
    }
}

fn install_database_guards(connection: &Connection) -> Result<()> {
    use rusqlite::hooks::{AuthAction, Authorization};
    connection
        .authorizer(Some(
            |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                AuthAction::Attach { .. }
                | AuthAction::Detach { .. }
                | AuthAction::Pragma { .. }
                | AuthAction::CreateVtable { .. }
                | AuthAction::DropVtable { .. }
                | AuthAction::CreateTempIndex { .. }
                | AuthAction::CreateTempTable { .. }
                | AuthAction::CreateTempTrigger { .. }
                | AuthAction::CreateTempView { .. }
                | AuthAction::DropTempIndex { .. }
                | AuthAction::DropTempTable { .. }
                | AuthAction::DropTempTrigger { .. }
                | AuthAction::DropTempView { .. }
                | AuthAction::Transaction { .. }
                | AuthAction::Savepoint { .. }
                | AuthAction::Unknown { .. } => Authorization::Deny,
                AuthAction::Function { function_name }
                    if matches!(
                        function_name.to_ascii_lowercase().as_str(),
                        "load_extension" | "randomblob" | "zeroblob" | "printf" | "format"
                    ) =>
                {
                    Authorization::Deny
                }
                _ => Authorization::Allow,
            },
        ))
        .map_err(db_error)?;
    let started = Instant::now();
    connection
        .progress_handler(1_000, Some(move || started.elapsed() > MAX_DATABASE_TIME))
        .map_err(db_error)?;
    Ok(())
}

fn json_to_sql_value(value: &Value) -> Result<rusqlite::types::Value> {
    use rusqlite::types::Value as SqlValue;
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(value) => Ok(SqlValue::Integer(i64::from(*value))),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                Ok(SqlValue::Integer(integer))
            } else if let Some(float) = value.as_f64() {
                Ok(SqlValue::Real(float))
            } else {
                Err(Error::Other(
                    "database number is outside SQLite range".into(),
                ))
            }
        }
        Value::String(value) if value.len() <= MAX_DATABASE_CELL_BYTES => {
            Ok(SqlValue::Text(value.clone()))
        }
        Value::Object(value) if value.len() == 1 && value.contains_key("blob_base64") => {
            let encoded = value["blob_base64"]
                .as_str()
                .ok_or_else(|| Error::Other("blob_base64 must be a string".into()))?;
            let decoded = crate::artifact::decode_base64(encoded)
                .ok_or_else(|| Error::Other("blob_base64 is invalid".into()))?;
            enforce_size(
                "database blob parameter",
                decoded.len(),
                MAX_DATABASE_CELL_BYTES,
            )?;
            Ok(SqlValue::Blob(decoded))
        }
        _ => Err(Error::Other(
            "database parameters must be null, boolean, number, string or blob_base64".into(),
        )),
    }
}

fn sql_to_json(value: rusqlite::types::ValueRef<'_>) -> Result<(Value, usize)> {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => Ok((Value::Null, 0)),
        ValueRef::Integer(value) => Ok((json!(value), 8)),
        ValueRef::Real(value) => Ok((json!(value), 8)),
        ValueRef::Text(value) => {
            enforce_size("database text cell", value.len(), MAX_DATABASE_CELL_BYTES)?;
            let text = std::str::from_utf8(value)
                .map_err(|_| Error::Other("database returned invalid UTF-8 text".into()))?;
            Ok((Value::String(text.to_string()), value.len()))
        }
        ValueRef::Blob(value) => {
            use base64::Engine;
            enforce_size("database blob cell", value.len(), MAX_DATABASE_CELL_BYTES)?;
            Ok((
                json!({
                    "blob_base64": base64::engine::general_purpose::STANDARD.encode(value)
                }),
                value.len(),
            ))
        }
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

fn enforce_additive_quota(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    size_expression: &str,
    incoming_bytes: u64,
    limit: u64,
) -> Result<()> {
    // Identifiers are fixed call-site constants, never request data.
    let sql = format!("SELECT COALESCE(SUM({size_expression}),0) FROM {table}");
    let used: i64 = tx.query_row(&sql, [], |row| row.get(0)).map_err(db_error)?;
    let projected = (used.max(0) as u64).saturating_add(incoming_bytes);
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

fn hash_site_password(password: &str) -> Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    if !(12..=128).contains(&password.len()) {
        return Err(Error::Other(
            "site password must contain 12 to 128 bytes".into(),
        ));
    }
    use rand::RngCore;
    let mut salt = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let salt = SaltString::encode_b64(&salt)
        .map_err(|e| Error::Other(format!("site password salt: {e}")))?;
    argon2::Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| Error::Other(format!("site password hash: {e}")))
}

fn verify_site_password(password: &str, encoded: &str) -> Result<bool> {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let hash =
        PasswordHash::new(encoded).map_err(|e| Error::Other(format!("site password hash: {e}")))?;
    Ok(argon2::Argon2::default()
        .verify_password(password.as_bytes(), &hash)
        .is_ok())
}

fn validate_site_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > MAX_KEY_BYTES
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains(['\0', '\\'])
        || path.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.starts_with('.')
                || part.len() > 128
        })
    {
        return Err(Error::Other("invalid or unsafe site path".into()));
    }
    Ok(())
}

fn site_media_type(path: &str) -> Result<&'static str> {
    let extension = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    match extension.as_deref() {
        Some("html" | "htm") => Ok("text/html; charset=utf-8"),
        Some("css") => Ok("text/css; charset=utf-8"),
        Some("js" | "mjs") => Ok("text/javascript; charset=utf-8"),
        Some("json") => Ok("application/json; charset=utf-8"),
        Some("txt") => Ok("text/plain; charset=utf-8"),
        Some("xml") => Ok("application/xml; charset=utf-8"),
        Some("svg") => Ok("image/svg+xml"),
        Some("png") => Ok("image/png"),
        Some("jpg" | "jpeg") => Ok("image/jpeg"),
        Some("gif") => Ok("image/gif"),
        Some("webp") => Ok("image/webp"),
        Some("avif") => Ok("image/avif"),
        Some("ico") => Ok("image/x-icon"),
        Some("woff") => Ok("font/woff"),
        Some("woff2") => Ok("font/woff2"),
        Some("ttf") => Ok("font/ttf"),
        _ => Err(Error::Other("site file type is not allowed".into())),
    }
}

fn scan_static_site_file(path: &str, content: &[u8]) -> Result<()> {
    let media = site_media_type(path)?;
    if !media.starts_with("text/")
        && !matches!(
            media,
            "application/json; charset=utf-8" | "application/xml; charset=utf-8" | "image/svg+xml"
        )
    {
        return Ok(());
    }
    let text = std::str::from_utf8(content)
        .map_err(|_| Error::Other("text site file must be valid UTF-8".into()))?;
    if text
        .bytes()
        .any(|byte| byte == 0 || byte < 0x09 || (byte > 0x0d && byte < 0x20))
    {
        return Err(Error::Other(
            "site security scan: control bytes are forbidden".into(),
        ));
    }
    if text.len() > 32 * 1024 && text.lines().any(|line| line.len() > 32 * 1024) {
        return Err(Error::Other(
            "site security scan: excessive minification or padding".into(),
        ));
    }
    let lower = text.to_ascii_lowercase();
    let forbidden = [
        ("-----begin private key", "embedded private key"),
        ("-----begin rsa private key", "embedded private key"),
        ("aws_secret_access_key=", "embedded cloud credential"),
        ("openai_api_key=", "embedded API credential"),
        ("github_token=", "embedded API credential"),
        ("<meta http-equiv=\"refresh\"", "automatic redirect"),
        ("<meta http-equiv='refresh'", "automatic redirect"),
        ("<base ", "base URL override"),
    ];
    for (pattern, reason) in forbidden {
        if lower.contains(pattern) {
            return Err(Error::Other(format!(
                "site security scan: forbidden {reason}"
            )));
        }
    }
    let encoded_markers = lower.matches("\\x").count()
        + lower.matches("\\u00").count()
        + lower.matches("fromcharcode").count()
        + lower.matches("atob(").count()
        + lower.matches("unescape(").count();
    if encoded_markers > 64 {
        return Err(Error::Other(
            "site security scan: excessive encoded or obfuscated content".into(),
        ));
    }
    Ok(())
}

fn site_version_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SiteVersion> {
    Ok(SiteVersion {
        version: row.get::<_, i64>(0)? as u64,
        active: row.get::<_, i64>(1)? != 0,
        file_count: row.get::<_, i64>(2)? as u64,
        size_bytes: row.get::<_, i64>(3)? as u64,
        created_at: row.get::<_, i64>(4)? as u64,
    })
}

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS kv(key TEXT PRIMARY KEY,value BLOB NOT NULL,expires_at INTEGER,updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS objects(object_key TEXT PRIMARY KEY,content BLOB NOT NULL,media_type TEXT NOT NULL,size_bytes INTEGER NOT NULL,digest TEXT NOT NULL,created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS functions(name TEXT PRIMARY KEY,active_version INTEGER,created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS function_versions(name TEXT NOT NULL,version INTEGER NOT NULL,runtime TEXT NOT NULL,source BLOB NOT NULL,digest TEXT NOT NULL,ruling TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(name,version),FOREIGN KEY(name) REFERENCES functions(name));
CREATE TABLE IF NOT EXISTS egress_hosts(host TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS function_http(name TEXT PRIMARY KEY,auth TEXT NOT NULL,cors_origins TEXT NOT NULL,FOREIGN KEY(name) REFERENCES functions(name) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS schedules(id TEXT PRIMARY KEY,function TEXT NOT NULL,cron TEXT NOT NULL,request TEXT NOT NULL,enabled INTEGER NOT NULL,next_run_at INTEGER NOT NULL,last_run_at INTEGER,last_status TEXT,FOREIGN KEY(function) REFERENCES functions(name) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS site_config(id INTEGER PRIMARY KEY CHECK(id=1),enabled INTEGER NOT NULL,entrypoint TEXT NOT NULL,spa_fallback INTEGER NOT NULL,username TEXT NOT NULL,password_hash TEXT NOT NULL,active_version INTEGER,updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS site_versions(version INTEGER PRIMARY KEY,created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS site_files(version INTEGER NOT NULL,path TEXT NOT NULL,content BLOB NOT NULL,media_type TEXT NOT NULL,size_bytes INTEGER NOT NULL,digest TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(version,path),FOREIGN KEY(version) REFERENCES site_versions(version) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS site_rate(second INTEGER PRIMARY KEY,requests INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS site_bandwidth(period INTEGER PRIMARY KEY,bytes INTEGER NOT NULL);
"#;

fn parse_minute_cron(cron: &str) -> Result<u64> {
    let parts: Vec<&str> = cron.split_whitespace().collect();
    if parts.len() != 5 || parts[1..] != ["*", "*", "*", "*"] {
        return Err(Error::Other(
            "cron must use the supported form */N * * * *".into(),
        ));
    }
    let minutes = parts[0]
        .strip_prefix("*/")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if !(1..=1440).contains(&minutes) {
        return Err(Error::Other(
            "cron interval must be between 1 and 1440 minutes".into(),
        ));
    }
    Ok(minutes)
}

fn schedule_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FunctionSchedule> {
    let request: String = row.get(3)?;
    Ok(FunctionSchedule {
        id: row.get(0)?,
        function: row.get(1)?,
        cron: row.get(2)?,
        request: serde_json::from_str(&request).unwrap_or(Value::Null),
        enabled: row.get::<_, i64>(4)? != 0,
        next_run_at: row.get::<_, i64>(5)? as u64,
        last_run_at: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        last_status: row.get(7)?,
    })
}

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
    fn function_security_scan_rejects_escape_obfuscation_and_http_fanout() {
        assert!(scan_javascript_function(b"async () => fetch('https://evil.test')").is_err());
        assert!(scan_javascript_function(b"() => process.env.SECRET").is_err());
        assert!(scan_javascript_function(
            b"async (_, gap) => { while (true) await gap.http.get('https://x.test'); }"
        )
        .is_err());
        let encoded = format!("() => '{}';", "\\x41".repeat(33));
        assert!(scan_javascript_function(encoded.as_bytes()).is_err());
    }

    #[test]
    fn function_security_scan_allows_bounded_brokered_http() {
        let findings = scan_javascript_function(
            b"async (request, gap) => gap.http.get('https://api.example.com/item')",
        )
        .unwrap();
        assert!(findings
            .iter()
            .any(|finding| finding.contains("outbound HTTP")));
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
    fn database_supports_parameterized_sql_and_blocks_file_escape() {
        let s = temp_store();
        s.database_execute(
            "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT, data BLOB)",
            &[],
        )
        .unwrap();
        let blob = json!({ "blob_base64": "AQID" });
        let inserted = s
            .database_execute(
                "INSERT INTO notes(body,data) VALUES(?1,?2)",
                &[json!("hello"), blob.clone()],
            )
            .unwrap();
        assert_eq!(inserted.affected_rows, 1);
        let queried = s
            .database_query(
                "SELECT id,body,data FROM notes WHERE body=?1",
                &[json!("hello")],
            )
            .unwrap();
        assert_eq!(queried.columns, ["id", "body", "data"]);
        assert_eq!(queried.rows[0], vec![json!(1), json!("hello"), blob]);
        assert!(s
            .database_query("ATTACH DATABASE '/tmp/escape.sqlite' AS escape", &[])
            .is_err());
        assert!(s.database_query("PRAGMA database_list", &[]).is_err());
        assert!(s
            .database_execute("CREATE TABLE a(x); CREATE TABLE b(x)", &[])
            .is_err());
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
        s.deploy_function("x", "javascript", b"a", 1).unwrap();
        let tx = s.control.transaction().unwrap();
        assert!(enforce_additive_quota(&tx, "function_versions", "length(source)", 1, 1).is_err());
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

    #[test]
    fn function_deletion_reclaims_versions_and_protects_the_active_one() {
        let mut s = temp_store();
        s.deploy_function("answer", "javascript", b"() => 41", 10)
            .unwrap();
        s.deploy_function("answer", "javascript", b"() => 42", 11)
            .unwrap();
        s.set_function_ruling("answer", 2, ReleaseRuling::Approved)
            .unwrap();
        s.activate_function("answer", 2).unwrap();

        assert!(s.delete_function_version("answer", 2).is_err());
        assert!(s.delete_function_version("answer", 1).unwrap());
        assert!(!s.delete_function_version("answer", 1).unwrap());
        assert!(s.delete_function("answer").unwrap());
        assert!(!s.delete_function("answer").unwrap());
        assert!(s.active_function("answer").unwrap().is_none());
        assert_eq!(
            s.control
                .query_row("SELECT COUNT(*) FROM function_versions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn private_site_versions_activate_atomically_and_authenticate() {
        let mut store = temp_store();
        let config = store
            .configure_site(
                true,
                "index.html",
                true,
                "visitor",
                Some("correct horse battery"),
                10,
            )
            .unwrap();
        assert_eq!(config.active_version, None);
        assert!(store
            .authenticate_site("visitor", "correct horse battery")
            .unwrap());
        assert!(!store
            .authenticate_site("visitor", "wrong password")
            .unwrap());
        assert!(!store
            .authenticate_site("other", "correct horse battery")
            .unwrap());

        let version = store.create_site_version(11).unwrap();
        assert_eq!(version.version, 1);
        store
            .put_site_file(1, "index.html", b"<!doctype html><body>Hello</body>", 12)
            .unwrap();
        store
            .put_site_file(1, "assets/app.js", b"document.body.dataset.ready='yes'", 12)
            .unwrap();
        assert_eq!(store.site_files(1).unwrap().len(), 2);
        assert!(store.delete_site_file(1, "assets/app.js").unwrap());
        assert_eq!(store.site_files(1).unwrap().len(), 1);
        store.activate_site_version(1).unwrap();
        assert_eq!(
            store.site_config().unwrap().unwrap().active_version,
            Some(1)
        );
        assert_eq!(
            store
                .active_site_asset("route/inside/spa")
                .unwrap()
                .unwrap()
                .content,
            b"<!doctype html><body>Hello</body>"
        );
        assert!(store.put_site_file(1, "late.css", b"body{}", 13).is_err());
        assert!(store.delete_site_version(1).is_err());
    }

    #[test]
    fn static_site_scan_blocks_secrets_redirects_and_unsafe_paths() {
        let mut store = temp_store();
        store
            .configure_site(
                true,
                "index.html",
                false,
                "demo",
                Some("a sufficiently long password"),
                1,
            )
            .unwrap();
        store.create_site_version(2).unwrap();
        assert!(store.put_site_file(1, ".env", b"SECRET=x", 3).is_err());
        assert!(store
            .put_site_file(1, "../index.html", b"hello", 3)
            .is_err());
        assert!(store.put_site_file(1, "payload.exe", b"MZ", 3).is_err());
        assert!(store
            .put_site_file(
                1,
                "index.html",
                br#"<meta http-equiv="refresh" content="0;url=https://evil.test">"#,
                3
            )
            .is_err());
        assert!(store
            .put_site_file(1, "app.js", b"const OPENAI_API_KEY='not allowed';", 3)
            .is_err());
        assert!(store
            .put_site_file(1, "huge.js", &vec![b'a'; MAX_SITE_FILE_BYTES + 1], 3)
            .is_err());
    }

    #[test]
    fn static_site_enforces_version_and_request_limits() {
        let mut store = temp_store();
        store
            .configure_site(
                true,
                "index.html",
                false,
                "demo",
                Some("a sufficiently long password"),
                1,
            )
            .unwrap();
        for _ in 0..MAX_SITE_VERSIONS {
            store.create_site_version(2).unwrap();
        }
        assert!(store.create_site_version(2).is_err());
        for _ in 0..MAX_SITE_REQUESTS_PER_SECOND {
            store.consume_site_quota(1, 100).unwrap();
        }
        assert!(store.consume_site_quota(1, 100).is_err());
    }
}
