//! Embedded state for Handfast.
//!
//! `redb` is a pure-Rust ACID key-value store. Keeping state in Rust avoids a
//! system SQLite dependency and keeps the Windows/Linux build identical. The
//! schema is versioned by table name; v2 P0 stores devices, plugin enablement,
//! settings and the daemon identity. Notification, contact and transfer tables
//! are added in the phases that use them.

#![deny(missing_docs)]

use std::path::Path;
use std::sync::Arc;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Device records, keyed by device ID, JSON-encoded.
const DEVICES: TableDefinition<&str, &[u8]> = TableDefinition::new("devices_v1");
/// Plugin enablement, keyed by `device_id + 0x1f + plugin`.
const PLUGIN_STATE: TableDefinition<&str, u8> = TableDefinition::new("plugin_state_v1");
/// Settings, keyed by name.
const SETTINGS: TableDefinition<&str, &str> = TableDefinition::new("settings_v1");

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

    /// A blocking store task failed to join.
    #[error("blocking task failed: {0}")]
    TaskJoin(String),
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

    /// Record a discovery sighting, preserving the stored pairing state.
    ///
    /// # Errors
    ///
    /// Fails when the existing record cannot be read, or the update cannot be
    /// serialized or written.
    pub fn record_discovery(
        &self,
        id: &str,
        name: &str,
        device_type: &str,
        last_seen: Option<i64>,
    ) -> Result<()> {
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(DEVICES)?;
            let paired = match table.get(id)? {
                Some(value) => serde_json::from_slice::<DeviceRecord>(value.value())?.paired,
                None => false,
            };
            let device = DeviceRecord {
                id: id.to_string(),
                name: name.to_string(),
                device_type: device_type.to_string(),
                paired,
                last_seen,
            };
            let value = serde_json::to_vec(&device)?;
            table.insert(id, value.as_slice())?;
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

    /// Set a device's pairing state.
    ///
    /// # Errors
    ///
    /// Fails when the read or write fails.
    pub fn set_paired(&self, id: &str, paired: bool) -> Result<bool> {
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(DEVICES)?;
            let Some(value) = table.get(id)? else {
                return Ok(false);
            };
            let mut device: DeviceRecord = serde_json::from_slice(value.value())?;
            drop(value);
            device.paired = paired;
            let value = serde_json::to_vec(&device)?;
            table.insert(id, value.as_slice())?;
        }
        write.commit()?;
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

    /// Run a blocking store operation on the blocking pool.
    ///
    /// Use this from async device tasks. Direct calls block the Tokio worker
    /// while redb waits for the write lock.
    ///
    /// # Errors
    ///
    /// Fails when the operation fails or the blocking task cannot be joined.
    pub async fn run_blocking<T, F>(self: Arc<Self>, operation: F) -> Result<T>
    where
        F: FnOnce(&Store) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        tokio::task::spawn_blocking(move || operation(&self))
            .await
            .map_err(|error| StoreError::TaskJoin(error.to_string()))?
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
        assert!(!store.device("a").unwrap().unwrap().paired);
        assert!(!store.set_paired("missing", true).unwrap());
    }

    #[test]
    fn discovery_update_preserves_pairing() {
        let (_directory, store) = test_store();
        store.upsert_device(&device("a", "Phone")).unwrap();
        assert!(store.set_paired("a", true).unwrap());

        store
            .record_discovery("a", "Renamed", "laptop", Some(1_700_000_100))
            .unwrap();

        let record = store.device("a").unwrap().unwrap();
        assert!(record.paired);
        assert_eq!(record.id, "a");
        assert_eq!(record.name, "Renamed");
        assert_eq!(record.device_type, "laptop");
        assert_eq!(record.last_seen, Some(1_700_000_100));
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

    #[tokio::test]
    async fn blocking_operations_can_run_off_the_async_worker() {
        let (_directory, store) = test_store();
        let store = Arc::new(store);
        store
            .clone()
            .run_blocking(|store| store.set_setting("mode", "async"))
            .await
            .unwrap();
        assert_eq!(store.setting("mode").unwrap().as_deref(), Some("async"));
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
