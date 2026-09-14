//! SQLCipher opening and one-time migration for node-owned SQLite files.

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

pub(crate) fn prepare_from_env() -> Result<usize, String> {
    let configured = std::env::var("GAP_MASTER_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let required = std::env::var("GAP_PROJECT_ENCRYPTION_REQUIRED")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        });
    let Some(configured) = configured else {
        return if required {
            Err("GAP_MASTER_KEY is required for encrypted node storage".into())
        } else {
            Ok(0)
        };
    };
    let master: [u8; 32] = hex::decode(configured.trim())
        .map_err(|_| "GAP_MASTER_KEY must be hex")?
        .try_into()
        .map_err(|_| "GAP_MASTER_KEY must be 32 bytes (64 hex chars)")?;
    let databases = [
        (
            "GAP_REGISTRATION_DB",
            "/data/registration.sqlite",
            "registration",
        ),
        (
            "GAP_ADMIN_CHALLENGES_DB",
            "/data/admin-challenges.sqlite",
            "admin-challenges",
        ),
        ("GAP_ADMIN_DB", "/data/cloud-admin.sqlite", "cloud-admin"),
        (
            "GAP_VM_SESSIONS_DB",
            "/data/cloud-vm-sessions.sqlite",
            "cloud-vm-sessions",
        ),
    ];
    let mut paths = std::collections::HashSet::new();
    let mut prepared = 0;
    for (variable, default_path, purpose) in databases {
        let configured_path = std::env::var(variable).unwrap_or_else(|_| default_path.into());
        let path = Path::new(&configured_path);
        if !path.exists() {
            continue;
        }
        if !paths.insert(path.to_path_buf()) {
            return Err(format!(
                "node databases must use distinct paths: {}",
                path.display()
            ));
        }
        drop(open(path, &master, purpose)?);
        prepared += 1;
    }
    Ok(prepared)
}

pub(crate) fn derive_key(master_key: &[u8; 32], purpose: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"gap-node-sqlcipher-v1\0");
    hash.update(master_key);
    hash.update(b"\0");
    hash.update(purpose.as_bytes());
    hex::encode(hash.finalize())
}

pub(crate) fn open(
    path: &Path,
    master_key: &[u8; 32],
    purpose: &str,
) -> Result<Connection, String> {
    let key = derive_key(master_key, purpose);
    if path != Path::new(":memory:") && plaintext(path)? {
        encrypt_plaintext(path, &key)?;
    }
    let connection =
        Connection::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    connection
        .pragma_update(None, "key", &key)
        .map_err(|error| format!("unlock {}: {error}", path.display()))?;
    connection
        .query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| format!("verify {}: {error}", path.display()))?;
    Ok(connection)
}

fn plaintext(path: &Path) -> Result<bool, String> {
    let Ok(mut file) = fs::File::open(path) else {
        return Ok(false);
    };
    let mut header = [0u8; 16];
    match file.read_exact(&mut header) {
        Ok(()) => Ok(&header == SQLITE_HEADER),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

fn encrypt_plaintext(path: &Path, key: &str) -> Result<(), String> {
    let source = Connection::open(path)
        .map_err(|error| format!("open plaintext {}: {error}", path.display()))?;
    source
        .busy_timeout(Duration::from_secs(10))
        .map_err(|error| format!("lock {}: {error}", path.display()))?;
    source
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|error| format!("checkpoint {}: {error}", path.display()))?;
    let temporary = path.with_extension("sqlite.encrypting");
    if temporary.exists() {
        fs::remove_file(&temporary)
            .map_err(|error| format!("remove {}: {error}", temporary.display()))?;
    }
    source
        .execute(
            "ATTACH DATABASE ?1 AS encrypted KEY ?2",
            params![temporary.to_string_lossy().as_ref(), key],
        )
        .map_err(|error| format!("attach {}: {error}", temporary.display()))?;
    let exported = source
        .query_row("SELECT sqlcipher_export('encrypted')", [], |_| Ok(()))
        .map_err(|error| format!("encrypt {}: {error}", path.display()));
    let detached = source
        .execute_batch("DETACH DATABASE encrypted")
        .map_err(|error| format!("detach {}: {error}", temporary.display()));
    exported?;
    detached?;
    drop(source);

    let encrypted = Connection::open(&temporary)
        .map_err(|error| format!("open {}: {error}", temporary.display()))?;
    encrypted
        .pragma_update(None, "key", key)
        .map_err(|error| format!("unlock {}: {error}", temporary.display()))?;
    encrypted
        .query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| format!("verify {}: {error}", temporary.display()))?;
    encrypted
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|error| format!("checkpoint {}: {error}", temporary.display()))?;
    drop(encrypted);

    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(&temporary, metadata.permissions())
            .map_err(|error| format!("chmod {}: {error}", temporary.display()))?;
    }
    OpenOptions::new()
        .write(true)
        .open(&temporary)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("sync {}: {error}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|error| format!("replace {}: {error}", path.display()))?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
        if sidecar.exists() {
            fs::remove_file(&sidecar)
                .map_err(|error| format!("remove {}: {error}", sidecar.display()))?;
        }
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync {}: {error}", parent.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_plaintext_and_refuses_wrong_key() {
        let root = std::env::temp_dir().join(crate::new_id("encrypted-sqlite-test"));
        fs::create_dir(&root).unwrap();
        let path = root.join("metadata.sqlite");
        let clear_database = Connection::open(&path).unwrap();
        clear_database
            .execute_batch(
                "CREATE TABLE values_(value TEXT); INSERT INTO values_ VALUES('preserved')",
            )
            .unwrap();
        drop(clear_database);

        let encrypted = open(&path, &[7; 32], "metadata").unwrap();
        let value: String = encrypted
            .query_row("SELECT value FROM values_", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "preserved");
        drop(encrypted);
        assert!(!plaintext(&path).unwrap());
        assert!(open(&path, &[8; 32], "metadata").is_err());
        assert_eq!(
            derive_key(&[7; 32], "metadata"),
            derive_key(&[7; 32], "metadata")
        );
        assert_ne!(
            derive_key(&[7; 32], "metadata"),
            derive_key(&[7; 32], "sessions")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
