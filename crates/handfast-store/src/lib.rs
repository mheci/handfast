//! Embedded state for Handfast.
//!
//! `redb` is a pure-Rust ACID key-value store. Keeping state in Rust avoids a
//! system SQLite dependency and keeps the Windows/Linux build identical. The
//! schema is versioned by table name; v2 P0 stores devices, plugin enablement,
//! settings and the daemon identity. Notification, contact and transfer tables
//! are added in the phases that use them.

#![deny(missing_docs)]

use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Device records, keyed by device ID, JSON-encoded.
const DEVICES: TableDefinition<&str, &[u8]> = TableDefinition::new("devices_v1");
/// Plugin enablement, keyed by `device_id + 0x1f + plugin`.
const PLUGIN_STATE: TableDefinition<&str, u8> = TableDefinition::new("plugin_state_v1");
/// Settings, keyed by name.
const SETTINGS: TableDefinition<&str, &str> = TableDefinition::new("settings_v1");
/// Daemon identity, singleton key `1`.
const IDENTITY: TableDefinition<u8, &[u8]> = TableDefinition::new("identity_v1");

/// Store errors.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Database open or close failed.
    #[error("database: {0}")]
    Database(#[from] redb::DatabaseError),

    /// A transaction could not start.
    #[error("transaction: {0}")]
    Transaction(#[from] redb::TransactionError),

    /// A table could not be opened.
    #[error("table: {0}")]
    Table(#[from] redb::TableError),

    /// A read or write failed.
    #[error("storage: {0}")]
    Storage(#[from] redb::StorageError),

    /// A commit failed.
    #[error("commit: {0}")]
    Commit(#[from] redb::CommitError),

    /// Stored JSON could not be decoded.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// The requested state is invalid.
    #[error("data: {0}")]
    Data(String),
}

/// Result alias for store operations.
pub type Result<T> = std::result::Result<T, StoreError>;

/// A known device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// Device identifier.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Device type string.
    pub device_type: String,
    /// Pairing state.
    pub paired: bool,
    /// Last seen Unix timestamp in seconds.
    pub last_seen: Option<i64>,
}

/// Persisted daemon identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityRecord {
    /// Device identifier.
    pub device_id: String,
    /// DER-encoded certificate.
    pub cert_der: Vec<u8>,
    /// DER-encoded private key.
    pub key_der: Vec<u8>,
}

/// Embedded state.
pub struct Store {
    database: Database,
}

impl Store {
    /// Open or create a database file and initialize tables.
    ///
    /// # Errors
    ///
    /// Fails when the database cannot be opened or initialized.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let database = Database::create(path)?;
        let store = Self { database };
        store.initialize()?;
        Ok(store)
    }

    /// Create every table if it does not exist.
    ///
    /// # Errors
    ///
    /// Fails when a transaction cannot be committed.
    pub fn initialize(&self) -> Result<()> {
        let write = self.database.begin_write()?;
        {
            let _ = write.open_table(DEVICES)?;
            let _ = write.open_table(PLUGIN_STATE)?;
            let _ = write.open_table(SETTINGS)?;
            let _ = write.open_table(IDENTITY)?;
        }
        write.commit()?;
        Ok(())
    }

    /// Insert or update a device.
    ///
    /// # Errors
    ///
    /// Fails when the record cannot be serialized or written.
    pub fn upsert_device(&self, device: &DeviceRecord) -> Result<()> {
        let value = serde_json::to_vec(device)?;
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(DEVICES)?;
            table.insert(device.id.as_str(), value.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }

    /// Read one device.
    ///
    /// # Errors
    ///
    /// Fails when the stored record cannot be decoded.
    pub fn device(&self, id: &str) -> Result<Option<DeviceRecord>> {
        let read = self.database.begin_read()?;
        let table = read.open_table(DEVICES)?;
        let Some(value) = table.get(id)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(value.value())?))
    }

    /// List devices ordered by name and ID.
    ///
    /// # Errors
    ///
    /// Fails when a stored record cannot be decoded.
    pub fn list_devices(&self) -> Result<Vec<DeviceRecord>> {
        let read = self.database.begin_read()?;
        let table = read.open_table(DEVICES)?;
        let mut devices = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            devices.push(serde_json::from_slice(value.value())?);
        }
        devices.sort_by(|left: &DeviceRecord, right: &DeviceRecord| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(devices)
    }

    /// Set a device's pairing state.
    ///
    /// # Errors
    ///
    /// Fails when the read or write fails.
    pub fn set_paired(&self, id: &str, paired: bool) -> Result<bool> {
        let Some(mut device) = self.device(id)? else {
            return Ok(false);
        };
        device.paired = paired;
        self.upsert_device(&device)?;
        Ok(true)
    }

    fn plugin_key(device_id: &str, plugin: &str) -> String {
        format!("{device_id}\u{1f}{plugin}")
    }

    /// Set per-device plugin enablement.
    ///
    /// # Errors
    ///
    /// Fails when the device does not exist or the write fails.
    pub fn set_plugin_enabled(&self, device_id: &str, plugin: &str, enabled: bool) -> Result<()> {
        if self.device(device_id)?.is_none() {
            return Err(StoreError::Data(format!("unknown device {device_id}")));
        }
        let key = Self::plugin_key(device_id, plugin);
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(PLUGIN_STATE)?;
            table.insert(key.as_str(), u8::from(enabled))?;
        }
        write.commit()?;
        Ok(())
    }

    /// Read per-device plugin enablement.
    ///
    /// # Errors
    ///
    /// Fails when the read fails.
    pub fn plugin_enabled(&self, device_id: &str, plugin: &str) -> Result<Option<bool>> {
        let key = Self::plugin_key(device_id, plugin);
        let read = self.database.begin_read()?;
        let table = read.open_table(PLUGIN_STATE)?;
        Ok(table.get(key.as_str())?.map(|value| value.value() != 0))
    }

    /// Read a setting.
    ///
    /// # Errors
    ///
    /// Fails when the read fails.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        let read = self.database.begin_read()?;
        let table = read.open_table(SETTINGS)?;
        Ok(table.get(key)?.map(|value| value.value().to_string()))
    }

    /// Write a setting.
    ///
    /// # Errors
    ///
    /// Fails when the write fails.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(SETTINGS)?;
            table.insert(key, value)?;
        }
        write.commit()?;
        Ok(())
    }

    /// Load the persisted daemon identity.
    ///
    /// # Errors
    ///
    /// Fails when the stored record cannot be decoded.
    pub fn load_identity(&self) -> Result<Option<IdentityRecord>> {
        let read = self.database.begin_read()?;
        let table = read.open_table(IDENTITY)?;
        let Some(value) = table.get(&1u8)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(value.value())?))
    }

    /// Store the daemon identity, replacing any previous value.
    ///
    /// # Errors
    ///
    /// Fails when the record cannot be serialized or written.
    pub fn save_identity(&self, record: &IdentityRecord) -> Result<()> {
        let value = serde_json::to_vec(record)?;
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(IDENTITY)?;
            table.insert(&1u8, value.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("state.db")).unwrap();
        (directory, store)
    }

    fn device(id: &str, name: &str) -> DeviceRecord {
        DeviceRecord {
            id: id.to_string(),
            name: name.to_string(),
            device_type: "phone".to_string(),
            paired: false,
            last_seen: Some(1_700_000_000),
        }
    }

    #[test]
    fn initialize_is_idempotent() {
        let (_directory, store) = test_store();
        store.initialize().unwrap();
        store.initialize().unwrap();
    }

    #[test]
    fn devices_round_trip() {
        let (_directory, store) = test_store();
        let mut record = device("a", "Phone");
        store.upsert_device(&record).unwrap();
        assert_eq!(store.device("a").unwrap(), Some(record.clone()));

        record.name = "Renamed".to_string();
        record.paired = true;
        store.upsert_device(&record).unwrap();
        assert_eq!(store.device("a").unwrap(), Some(record));

        assert!(store.set_paired("a", false).unwrap());
        assert!(!store.set_paired("missing", true).unwrap());
        assert_eq!(store.list_devices().unwrap().len(), 1);
    }

    #[test]
    fn device_list_is_sorted_by_name() {
        let (_directory, store) = test_store();
        store.upsert_device(&device("b", "Zulu")).unwrap();
        store.upsert_device(&device("a", "Alpha")).unwrap();
        let names: Vec<String> = store
            .list_devices()
            .unwrap()
            .into_iter()
            .map(|record| record.name)
            .collect();
        assert_eq!(names, vec!["Alpha", "Zulu"]);
    }

    #[test]
    fn plugin_state_is_per_device() {
        let (_directory, store) = test_store();
        store.upsert_device(&device("a", "Phone")).unwrap();
        assert_eq!(store.plugin_enabled("a", "ping").unwrap(), None);
        store.set_plugin_enabled("a", "ping", false).unwrap();
        assert_eq!(store.plugin_enabled("a", "ping").unwrap(), Some(false));
        store.set_plugin_enabled("a", "ping", true).unwrap();
        assert_eq!(store.plugin_enabled("a", "ping").unwrap(), Some(true));
    }

    #[test]
    fn plugin_state_requires_known_device() {
        let (_directory, store) = test_store();
        assert!(store.set_plugin_enabled("missing", "ping", true).is_err());
    }

    #[test]
    fn settings_round_trip() {
        let (_directory, store) = test_store();
        assert_eq!(store.setting("device_id").unwrap(), None);
        store.set_setting("device_id", "abc").unwrap();
        assert_eq!(store.setting("device_id").unwrap().as_deref(), Some("abc"));
        store.set_setting("device_id", "def").unwrap();
        assert_eq!(store.setting("device_id").unwrap().as_deref(), Some("def"));
    }

    #[test]
    fn identity_round_trip() {
        let (_directory, store) = test_store();
        let record = IdentityRecord {
            device_id: "a".repeat(32),
            cert_der: vec![1, 2, 3],
            key_der: vec![4, 5, 6],
        };
        assert_eq!(store.load_identity().unwrap(), None);
        store.save_identity(&record).unwrap();
        assert_eq!(store.load_identity().unwrap(), Some(record));
    }

    #[test]
    fn file_store_persists() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        {
            let store = Store::open(&path).unwrap();
            store.upsert_device(&device("a", "Phone")).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.device("a").unwrap().unwrap().name, "Phone");
    }
}
