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

use redb::{Database, ReadableDatabase, ReadableTable, Table, TableDefinition};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// Device records, keyed by device ID, JSON-encoded.
const DEVICES: TableDefinition<&str, &[u8]> = TableDefinition::new("devices_v1");
const MAX_UNTRUSTED_DEVICES: usize = 1024;
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
    /// Legacy display flag. Trust is the certificate pin, not this field.
    pub paired: bool,
    /// Last seen Unix timestamp in seconds.
    pub last_seen: Option<i64>,
    /// SHA-256 of the peer's DER certificate, the only cryptographic trust.
    #[serde(default)]
    pub certificate_fingerprint: Option<[u8; 32]>,
}

/// Persisted daemon identity. The key is private material, so this must not
/// implement `Debug`, and the key is zeroized on drop.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct IdentityRecord {
    /// Device identifier.
    pub device_id: String,
    /// DER-encoded certificate.
    pub cert_der: Vec<u8>,
    /// DER-encoded private key.
    pub key_der: Vec<u8>,
}

impl<'de> Deserialize<'de> for IdentityRecord {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            device_id: String,
            cert_der: Vec<u8>,
            #[serde(deserialize_with = "deserialize_key_der")]
            key_der: Zeroizing<Vec<u8>>,
        }

        let Wire {
            device_id,
            cert_der,
            mut key_der,
        } = Wire::deserialize(deserializer)?;
        Ok(Self {
            device_id,
            cert_der,
            key_der: std::mem::take(&mut *key_der),
        })
    }
}

/// Read the key into a buffer that zeroizes when decoding fails part way.
fn deserialize_key_der<'de, D>(deserializer: D) -> std::result::Result<Zeroizing<Vec<u8>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct KeyVisitor;

    impl<'de> serde::de::Visitor<'de> for KeyVisitor {
        type Value = Zeroizing<Vec<u8>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a byte array")
        }

        fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut key = Zeroizing::new(Vec::new());
            while let Some(byte) = seq.next_element::<u8>()? {
                key.push(byte);
            }
            Ok(key)
        }
    }

    deserializer.deserialize_seq(KeyVisitor)
}

impl Zeroize for IdentityRecord {
    fn zeroize(&mut self) {
        self.key_der.zeroize();
    }
}

impl Drop for IdentityRecord {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl ZeroizeOnDrop for IdentityRecord {}

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

    /// Record a discovery sighting. Discovery is unauthenticated, so an
    /// existing record only gets a fresh `last_seen`. Name, type, pairing and
    /// the trust pin change only through the authenticated paths.
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
            let existing = match table.get(id)? {
                Some(value) => {
                    let mut device: DeviceRecord = serde_json::from_slice(value.value())?;
                    device.last_seen = last_seen;
                    Some(device)
                }
                None => None,
            };
            match existing {
                Some(device) => {
                    let value = serde_json::to_vec(&device)?;
                    table.insert(id, value.as_slice())?;
                }
                None => {
                    Self::insert_untrusted_device(&mut table, id, name, device_type, last_seen)?;
                }
            }
        }
        write.commit()?;
        Ok(())
    }

    fn evict_oldest_untrusted(table: &mut Table<'_, &str, &[u8]>) -> Result<()> {
        let mut untrusted = 0usize;
        let mut oldest: Option<(String, Option<i64>)> = None;
        for entry in table.iter()? {
            let (key, value) = entry?;
            let device: DeviceRecord = serde_json::from_slice(value.value())?;
            if device.paired || device.certificate_fingerprint.is_some() {
                continue;
            }
            untrusted += 1;
            if oldest
                .as_ref()
                .is_none_or(|(_, last_seen)| device.last_seen < *last_seen)
            {
                oldest = Some((key.value().to_string(), device.last_seen));
            }
        }
        if untrusted < MAX_UNTRUSTED_DEVICES {
            return Ok(());
        }
        if let Some((oldest, _)) = oldest {
            table.remove(oldest.as_str())?;
        }
        Ok(())
    }

    fn insert_untrusted_device(
        table: &mut Table<'_, &str, &[u8]>,
        id: &str,
        name: &str,
        device_type: &str,
        last_seen: Option<i64>,
    ) -> Result<()> {
        Self::evict_oldest_untrusted(table)?;
        let value = serde_json::to_vec(&DeviceRecord {
            id: id.to_string(),
            name: name.to_string(),
            device_type: device_type.to_string(),
            paired: false,
            last_seen,
            certificate_fingerprint: None,
        })?;
        table.insert(id, value.as_slice())?;
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

    /// Load the persisted daemon identity.
    ///
    /// # Errors
    ///
    /// Fails when the stored record cannot be decoded.
    pub fn identity(&self) -> Result<Option<IdentityRecord>> {
        let read = self.database.begin_read()?;
        let table = read.open_table(IDENTITY)?;
        let Some(value) = table.get(&1u8)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(value.value())?))
    }

    /// Serialize the identity into a buffer that is zeroized on drop.
    fn serialize_identity(record: &IdentityRecord) -> Result<Zeroizing<Vec<u8>>> {
        Ok(Zeroizing::new(serde_json::to_vec(record)?))
    }

    /// Store the daemon identity, replacing any previous value.
    ///
    /// # Errors
    ///
    /// Fails when the record cannot be serialized or written.
    pub fn set_identity(&self, record: &IdentityRecord) -> Result<()> {
        let value = Self::serialize_identity(record)?;
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(IDENTITY)?;
            table.insert(&1u8, value.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }

    /// Set or clear a device's certificate pin. The legacy pairing flag is
    /// derived from the pin, and both fields are written in one transaction.
    ///
    /// # Errors
    ///
    /// Fails when the read or write fails.
    pub fn set_trusted(&self, id: &str, pin: Option<[u8; 32]>) -> Result<bool> {
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(DEVICES)?;
            let Some(value) = table.get(id)? else {
                return Ok(false);
            };
            let mut device: DeviceRecord = serde_json::from_slice(value.value())?;
            drop(value);
            device.paired = pin.is_some();
            device.certificate_fingerprint = pin;
            let value = serde_json::to_vec(&device)?;
            table.insert(id, value.as_slice())?;
        }
        write.commit()?;
        Ok(true)
    }

    /// List every device with a trust pin. A record with `paired = true` and no
    /// pin is not trusted and is absent.
    ///
    /// # Errors
    ///
    /// Fails when the read fails or a stored record cannot be decoded.
    pub fn trust_table(&self) -> Result<Vec<(String, [u8; 32])>> {
        let read = self.database.begin_read()?;
        let table = read.open_table(DEVICES)?;
        let mut pins = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            let device: DeviceRecord = serde_json::from_slice(value.value())?;
            if let Some(pin) = device.certificate_fingerprint {
                pins.push((key.value().to_string(), pin));
            }
        }
        Ok(pins)
    }

    /// Update a device's name and type from an authenticated identity. Trust
    /// and `last_seen` are preserved, and an unknown device is inserted
    /// unpaired.
    ///
    /// # Errors
    ///
    /// Fails when the record cannot be decoded or written.
    pub fn record_authenticated_identity(
        &self,
        id: &str,
        name: &str,
        device_type: &str,
    ) -> Result<()> {
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(DEVICES)?;
            let existing = match table.get(id)? {
                Some(value) => {
                    let mut device: DeviceRecord = serde_json::from_slice(value.value())?;
                    device.name = name.to_string();
                    device.device_type = device_type.to_string();
                    Some(device)
                }
                None => None,
            };
            match existing {
                Some(device) => {
                    let value = serde_json::to_vec(&device)?;
                    table.insert(id, value.as_slice())?;
                }
                None => {
                    Self::insert_untrusted_device(&mut table, id, name, device_type, None)?;
                }
            }
        }
        write.commit()?;
        Ok(())
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
            certificate_fingerprint: None,
        }
    }

    fn identity_record() -> IdentityRecord {
        IdentityRecord {
            device_id: "a".repeat(32),
            cert_der: vec![1, 2, 3],
            key_der: vec![4, 5, 6],
        }
    }

    /// Write the v0.2.0 schema: devices, plugin state and settings, with no
    /// `identity_v1` table and no `certificate_fingerprint` field.
    fn write_v020_state(path: &Path, paired: bool) {
        let database = Database::create(path).unwrap();
        let write = database.begin_write().unwrap();
        {
            let device_id = "a".repeat(32);
            let mut devices = write.open_table(DEVICES).unwrap();
            let record = serde_json::json!({
                "id": "a",
                "name": "Phone",
                "device_type": "phone",
                "paired": paired,
                "last_seen": 1_700_000_000_i64,
            });
            let value = serde_json::to_vec(&record).unwrap();
            devices.insert("a", value.as_slice()).unwrap();

            let mut plugins = write.open_table(PLUGIN_STATE).unwrap();
            plugins.insert("a\u{1f}ping", 1).unwrap();

            let mut settings = write.open_table(SETTINGS).unwrap();
            settings.insert("device_id", device_id.as_str()).unwrap();
        }
        write.commit().unwrap();
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
    }

    #[test]
    fn discovery_update_preserves_pairing_trust_and_identity() {
        let (_directory, store) = test_store();
        store.upsert_device(&device("a", "Phone")).unwrap();
        let pin = [9u8; 32];
        assert!(store.set_trusted("a", Some(pin)).unwrap());

        store
            .record_discovery("a", "Spoofed name", "laptop", Some(1_700_000_100))
            .unwrap();

        let record = store.device("a").unwrap().unwrap();
        assert!(record.paired);
        assert_eq!(record.certificate_fingerprint, Some(pin));
        assert_eq!(record.id, "a");
        assert_eq!(record.name, "Phone");
        assert_eq!(record.device_type, "phone");
        assert_eq!(record.last_seen, Some(1_700_000_100));
    }

    #[test]
    fn discovery_inserts_an_unpaired_device() {
        let (_directory, store) = test_store();
        store
            .record_discovery("b", "Tablet", "tablet", Some(1_700_000_200))
            .unwrap();

        let record = store.device("b").unwrap().unwrap();
        assert_eq!(record.name, "Tablet");
        assert_eq!(record.device_type, "tablet");
        assert!(!record.paired);
        assert_eq!(record.certificate_fingerprint, None);
        assert_eq!(record.last_seen, Some(1_700_000_200));
    }

    fn untrusted_id(index: usize) -> String {
        format!("u{index:031x}")
    }

    #[test]
    fn discovery_evicts_the_oldest_untrusted_device_at_the_cap() {
        let (_directory, store) = test_store();

        let mut paired = device("paired", "Paired");
        paired.paired = true;
        paired.last_seen = Some(0);
        store.upsert_device(&paired).unwrap();

        let mut trusted = device("trusted", "Trusted");
        trusted.last_seen = Some(0);
        store.upsert_device(&trusted).unwrap();
        assert!(store.set_trusted("trusted", Some([4u8; 32])).unwrap());

        for index in 0..=MAX_UNTRUSTED_DEVICES {
            store
                .record_discovery(
                    &untrusted_id(index),
                    "Phone",
                    "phone",
                    Some(index as i64 + 1),
                )
                .unwrap();
        }

        assert!(
            store.device(&untrusted_id(0)).unwrap().is_none(),
            "the oldest unpaired device is evicted"
        );
        let next = store.device(&untrusted_id(1)).unwrap().unwrap();
        assert_eq!(next.last_seen, Some(2));
        let newest = store
            .device(&untrusted_id(MAX_UNTRUSTED_DEVICES))
            .unwrap()
            .unwrap();
        assert_eq!(newest.last_seen, Some(MAX_UNTRUSTED_DEVICES as i64 + 1));
        assert!(
            store.device("paired").unwrap().unwrap().paired,
            "a paired row older than every unpaired row is kept"
        );
        let trusted = store.device("trusted").unwrap().unwrap();
        assert_eq!(trusted.certificate_fingerprint, Some([4u8; 32]));
    }

    #[test]
    fn authenticated_identity_insert_evicts_the_oldest_untrusted_device() {
        let (_directory, store) = test_store();

        let mut paired = device("paired", "Paired");
        paired.paired = true;
        paired.last_seen = Some(0);
        store.upsert_device(&paired).unwrap();

        let mut trusted = device("trusted", "Trusted");
        trusted.last_seen = Some(0);
        store.upsert_device(&trusted).unwrap();
        assert!(store.set_trusted("trusted", Some([5u8; 32])).unwrap());

        for index in 0..MAX_UNTRUSTED_DEVICES {
            store
                .record_discovery(
                    &untrusted_id(index),
                    "Phone",
                    "phone",
                    Some(index as i64 + 1),
                )
                .unwrap();
        }

        store
            .record_authenticated_identity("authenticated", "New phone", "phone")
            .unwrap();

        assert!(
            store.device(&untrusted_id(0)).unwrap().is_none(),
            "the oldest unpaired, unpinned device is evicted"
        );
        assert!(store.device(&untrusted_id(1)).unwrap().is_some());
        let inserted = store.device("authenticated").unwrap().unwrap();
        assert_eq!(inserted.name, "New phone");
        assert!(!inserted.paired);
        assert_eq!(inserted.certificate_fingerprint, None);
        assert!(
            store.device("paired").unwrap().unwrap().paired,
            "a paired row is kept"
        );
        assert_eq!(
            store
                .device("trusted")
                .unwrap()
                .unwrap()
                .certificate_fingerprint,
            Some([5u8; 32]),
            "a pinned row is kept"
        );
    }

    #[test]
    fn trust_writes_pairing_and_pin_together() {
        let (_directory, store) = test_store();
        store.upsert_device(&device("a", "Phone")).unwrap();
        let pin = [7u8; 32];

        assert!(store.set_trusted("a", Some(pin)).unwrap());
        let record = store.device("a").unwrap().unwrap();
        assert!(record.paired);
        assert_eq!(record.certificate_fingerprint, Some(pin));
        assert_eq!(store.trust_table().unwrap(), vec![("a".to_string(), pin)]);

        assert!(store.set_trusted("a", None).unwrap());
        let record = store.device("a").unwrap().unwrap();
        assert!(!record.paired);
        assert_eq!(record.certificate_fingerprint, None);
        assert!(store.trust_table().unwrap().is_empty());

        assert!(!store.set_trusted("missing", Some(pin)).unwrap());
    }

    #[test]
    fn authenticated_identity_updates_name_and_type_without_clearing_trust() {
        let (_directory, store) = test_store();
        store.upsert_device(&device("a", "Phone")).unwrap();
        let pin = [3u8; 32];
        assert!(store.set_trusted("a", Some(pin)).unwrap());

        store
            .record_authenticated_identity("a", "Phone (TLS)", "laptop")
            .unwrap();

        let record = store.device("a").unwrap().unwrap();
        assert_eq!(record.name, "Phone (TLS)");
        assert_eq!(record.device_type, "laptop");
        assert!(record.paired);
        assert_eq!(record.certificate_fingerprint, Some(pin));
        assert_eq!(record.last_seen, Some(1_700_000_000));

        store
            .record_authenticated_identity("b", "New phone", "phone")
            .unwrap();
        let record = store.device("b").unwrap().unwrap();
        assert_eq!(record.name, "New phone");
        assert!(!record.paired);
        assert_eq!(record.certificate_fingerprint, None);
        assert_eq!(record.last_seen, None);
    }

    #[test]
    #[allow(unsafe_code)]
    fn identity_record_zeroizes_its_key_on_drop() {
        // Observing freed memory is unsafe, so this checks the function Drop
        // calls and the marker trait that guards the contract.
        fn assert_zeroize_on_drop<T: ZeroizeOnDrop>() {}
        assert_zeroize_on_drop::<IdentityRecord>();

        let mut record = identity_record();
        let key_len = record.key_der.len();
        record.zeroize();
        assert!(record.key_der.is_empty(), "zeroize clears the key Vec");
        let zeroed = record.key_der.spare_capacity_mut();
        assert!(
            zeroed.len() >= key_len,
            "the clear exposes the key allocation"
        );
        for (index, byte) in zeroed[..key_len].iter().enumerate() {
            // SAFETY: zeroize wrote zeros to the first key_len bytes before it
            // cleared the Vec, and the Vec still owns the allocation.
            assert_eq!(
                unsafe { *byte.assume_init_ref() },
                0,
                "key byte {index} is zero"
            );
        }
    }

    #[test]
    fn identity_round_trips_and_persists_across_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let record = identity_record();
        {
            let store = Store::open(&path).unwrap();
            assert!(store.identity().unwrap().is_none());
            store.set_identity(&record).unwrap();

            let loaded = store.identity().unwrap().expect("saved identity loads");
            assert_eq!(loaded.device_id, record.device_id);
            assert_eq!(loaded.cert_der, record.cert_der);
            assert_eq!(loaded.key_der, record.key_der);
        }

        let store = Store::open(&path).unwrap();
        let loaded = store.identity().unwrap().expect("identity survives reopen");
        assert_eq!(loaded.device_id, record.device_id);
        assert_eq!(loaded.cert_der, record.cert_der);
        assert_eq!(loaded.key_der, record.key_der);
    }

    #[test]
    fn identity_serialization_buffer_is_zeroizing() {
        // The buffer type is the guard: any early return in set_identity drops
        // it through Zeroizing, which zeroizes the serialized key.
        let record = identity_record();
        let buffer: Zeroizing<Vec<u8>> = Store::serialize_identity(&record).unwrap();
        assert_eq!(
            buffer.as_slice(),
            serde_json::to_vec(&record).unwrap().as_slice(),
            "the zeroizing buffer carries the bytes the store writes"
        );
    }

    #[test]
    fn identity_schema_round_trips_unchanged() {
        let record = identity_record();
        let json = serde_json::json!({
            "device_id": record.device_id.as_str(),
            "cert_der": [1, 2, 3],
            "key_der": [4, 5, 6],
        });
        assert_eq!(serde_json::to_value(&record).unwrap(), json);
        let decoded: IdentityRecord = serde_json::from_value(json).unwrap();
        assert!(decoded == record, "round trip preserves the record");
    }

    #[test]
    fn malformed_identity_record_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        {
            let database = Database::create(&path).unwrap();
            let write = database.begin_write().unwrap();
            {
                let malformed = br#"{"device_id":"a","key_der":[4,5,6],"cert_der":"not bytes"}"#;
                let mut identity = write.open_table(IDENTITY).unwrap();
                identity.insert(&1u8, &malformed[..]).unwrap();
            }
            write.commit().unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert!(matches!(store.identity(), Err(StoreError::Json(_))));
    }

    #[test]
    fn malformed_identity_key_is_rejected() {
        let record = br#"{"device_id":"a","cert_der":[1],"key_der":[4,5,300]}"#;
        assert!(serde_json::from_slice::<IdentityRecord>(record).is_err());
    }

    #[test]
    fn v020_state_opens_without_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        write_v020_state(&path, false);

        let store = Store::open(&path).unwrap();
        // identity() opens the table, so this also proves initialize created it.
        assert!(store.identity().unwrap().is_none());
        let device_id = "a".repeat(32);
        assert_eq!(
            store.setting("device_id").unwrap().as_deref(),
            Some(device_id.as_str())
        );
        assert_eq!(store.plugin_enabled("a", "ping").unwrap(), Some(true));
        let record = store.device("a").unwrap().expect("device survives");
        assert_eq!(record.name, "Phone");
        assert_eq!(record.certificate_fingerprint, None);
    }

    #[test]
    fn v020_paired_device_without_pin_is_untrusted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        write_v020_state(&path, true);

        let store = Store::open(&path).unwrap();
        let record = store.device("a").unwrap().unwrap();
        assert!(record.paired);
        assert_eq!(record.certificate_fingerprint, None);
        assert!(store.trust_table().unwrap().is_empty());
    }

    #[test]
    fn pre_audit_identity_v1_loads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let device_id = "b".repeat(32);
        {
            let database = Database::create(&path).unwrap();
            let write = database.begin_write().unwrap();
            {
                let record = serde_json::json!({
                    "device_id": device_id.as_str(),
                    "cert_der": [1, 2, 3],
                    "key_der": [4, 5, 6],
                });
                let value = serde_json::to_vec(&record).unwrap();
                let mut identity = write.open_table(IDENTITY).unwrap();
                identity.insert(&1u8, value.as_slice()).unwrap();

                let mut settings = write.open_table(SETTINGS).unwrap();
                settings.insert("device_id", device_id.as_str()).unwrap();
            }
            write.commit().unwrap();
        }

        let store = Store::open(&path).unwrap();
        let record = store.identity().unwrap().expect("pre-audit identity loads");
        assert_eq!(record.device_id, device_id);
        assert_eq!(record.cert_der, vec![1, 2, 3]);
        assert_eq!(record.key_der, vec![4, 5, 6]);
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
