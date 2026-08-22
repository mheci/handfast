//! SQLite-backed persistent state.
//!
//! [`Store`] keeps the paired-device registry plus a small key/value table for
//! settings that must survive restarts (identity material references, UI
//! toggles, ...). Connections are short-lived from the caller's perspective:
//! every method takes `&self` and locks internally, which is fine for
//! daemon-scale traffic and avoids async-mutex poisoning concerns entirely.
//!
//! The module also provides [`atomic_write`], a crash-safe file replacement
//! helper used to persist certificates and other small blobs.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A persisted device record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRow {
    /// Stable device identifier.
    pub device_id: String,
    /// Human-readable device name.
    pub name: String,
    /// Device class label (`"phone"`, `"desktop"`, ...).
    pub device_type: String,
    /// SHA-256 fingerprint of the device certificate.
    pub cert_fingerprint: String,
    /// Whether the pairing handshake completed.
    pub paired: bool,
    /// Unix timestamp of the last sighting, if any.
    pub last_seen: Option<i64>,
}

/// Handle to the on-disk SQLite database.
#[derive(Debug)]
pub struct Store {
    conn: Mutex<Connection>,
}

/// Schema created idempotently on open.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS devices (
    device_id        TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    device_type      TEXT NOT NULL,
    cert_fingerprint TEXT NOT NULL,
    paired           INTEGER NOT NULL DEFAULT 0,
    last_seen        INTEGER
);
CREATE TABLE IF NOT EXISTS kv (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
";

/// Convert a rusqlite error into the crate error type.
fn sqlite_err(err: rusqlite::Error) -> Error {
    Error::Sqlite(err.to_string())
}

/// Lock a possibly poisoned mutex, recovering the guard regardless of poison.
///
/// A panic in one query must not permanently brick the store; the affected
/// transaction was rolled back by rusqlite's Drop impl anyway.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Store {
    /// Open (creating if necessary) the database at `path`.
    ///
    /// Enables WAL journalling for crash-safe concurrent reads and creates the
    /// `devices` and `kv` tables when missing. Parent directories must exist.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(sqlite_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(sqlite_err)?;
        conn.pragma_update(None, "busy_timeout", 5_000)
            .map_err(sqlite_err)?;
        conn.execute_batch(SCHEMA).map_err(sqlite_err)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// All known devices ordered by identifier.
    pub fn list_devices(&self) -> Result<Vec<DeviceRow>> {
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT device_id, name, device_type, cert_fingerprint, paired, last_seen \
                 FROM devices ORDER BY device_id",
            )
            .map_err(sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(DeviceRow {
                    device_id: row.get(0)?,
                    name: row.get(1)?,
                    device_type: row.get(2)?,
                    cert_fingerprint: row.get(3)?,
                    paired: row.get::<_, i64>(4)? != 0,
                    last_seen: row.get(5)?,
                })
            })
            .map_err(sqlite_err)?;

        let mut devices = Vec::new();
        for row in rows {
            devices.push(row.map_err(sqlite_err)?);
        }
        Ok(devices)
    }

    /// Insert or update a device record keyed by `device_id`.
    pub fn upsert_device(&self, d: &DeviceRow) -> Result<()> {
        let conn = lock(&self.conn);
        conn.execute(
            "INSERT INTO devices \
                 (device_id, name, device_type, cert_fingerprint, paired, last_seen) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(device_id) DO UPDATE SET \
                name = excluded.name, \
                device_type = excluded.device_type, \
                cert_fingerprint = excluded.cert_fingerprint, \
                paired = excluded.paired, \
                last_seen = excluded.last_seen",
            params![
                d.device_id,
                d.name,
                d.device_type,
                d.cert_fingerprint,
                i64::from(d.paired),
                d.last_seen
            ],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    /// Remove a device record; deleting an unknown id is a no-op.
    pub fn delete_device(&self, id: &str) -> Result<()> {
        let conn = lock(&self.conn);
        conn.execute("DELETE FROM devices WHERE device_id = ?1", params![id])
            .map_err(sqlite_err)?;
        Ok(())
    }

    /// Fetch a key/value entry, or `None` when absent.
    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let conn = lock(&self.conn);
        conn.query_row("SELECT v FROM kv WHERE k = ?1", params![key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(sqlite_err)
    }

    /// Insert or overwrite a key/value entry.
    pub fn kv_set(&self, key: &str, val: &str) -> Result<()> {
        let conn = lock(&self.conn);
        conn.execute(
            "INSERT INTO kv (k, v) VALUES (?1, ?2) \
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![key, val],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }
}

/// Atomically replace the file at `path` with `bytes`.
///
/// The payload is written to a uniquely named temporary sibling, fsynced, then
/// renamed over `path`; on Unix the parent directory is fsynced afterwards so
/// the rename itself survives a power cut. A failed attempt leaves the original
/// file untouched and cleans up its temporary file.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map_or_else(|| "blob".to_string(), |n| n.to_string_lossy().into_owned());
    let tmp = parent.join(format!(
        ".{file_name}.tmp-{}",
        uuid::Uuid::new_v4().simple()
    ));

    let write_result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();

    if write_result.is_err() {
        // Best effort; a leftover temp file is harmless but untidy.
        let _ = std::fs::remove_file(&tmp);
    }

    #[cfg(unix)]
    if write_result.is_ok() {
        // Make the rename durable; failure here only weakens durability.
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp_store(dir: &Path) -> Store {
        Store::open(&dir.join("state.db")).expect("store open should succeed")
    }

    #[test]
    fn device_roundtrip_upsert_and_delete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open_temp_store(dir.path());
        assert_eq!(store.list_devices().expect("list"), Vec::new());

        let device = DeviceRow {
            device_id: "dev-a".to_string(),
            name: "Phone".to_string(),
            device_type: "phone".to_string(),
            cert_fingerprint: "aa:bb:cc".to_string(),
            paired: true,
            last_seen: Some(1_700_000_000),
        };
        store.upsert_device(&device).expect("upsert");

        let mut updated = device.clone();
        updated.name = "Renamed".to_string();
        updated.paired = false;
        updated.last_seen = None;
        store.upsert_device(&updated).expect("re-upsert");

        assert_eq!(store.list_devices().expect("list"), vec![updated]);

        store.delete_device("dev-a").expect("delete");
        assert_eq!(store.list_devices().expect("list"), Vec::new());
        // Deleting an unknown id is a no-op, not an error.
        store.delete_device("dev-a").expect("delete again");
    }

    #[test]
    fn kv_roundtrip_and_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open_temp_store(dir.path());

        assert_eq!(store.kv_get("missing").expect("get"), None);
        store.kv_set("key", "one").expect("set");
        assert_eq!(store.kv_get("key").expect("get"), Some("one".to_string()));
        store.kv_set("key", "two").expect("overwrite");
        assert_eq!(store.kv_get("key").expect("get"), Some("two".to_string()));
    }

    #[test]
    fn store_reopens_with_data_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let store = open_temp_store(dir.path());
            store
                .upsert_device(&DeviceRow {
                    device_id: "dev-b".to_string(),
                    name: "Laptop".to_string(),
                    device_type: "laptop".to_string(),
                    cert_fingerprint: "dd:ee".to_string(),
                    paired: false,
                    last_seen: None,
                })
                .expect("upsert");
            store.kv_set("greeting", "hello").expect("set");
        }
        // Reopening exercises CREATE TABLE IF NOT EXISTS against existing data.
        let reopened = open_temp_store(dir.path());
        assert_eq!(reopened.list_devices().expect("list").len(), 1);
        assert_eq!(
            reopened.kv_get("greeting").expect("get"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn atomic_write_creates_overwrites_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("out.json");

        atomic_write(&target, b"first").expect("initial write");
        assert_eq!(std::fs::read(&target).expect("read"), b"first");

        atomic_write(&target, b"second-payload").expect("overwrite");
        assert_eq!(std::fs::read(&target).expect("read"), b"second-payload");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");
    }
}
