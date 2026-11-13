//! Local identity, certificate persistence and state-file permissions.
//!
//! A stored identity is never regenerated. A corrupt record, a common-name
//! mismatch or a device ID that disagrees with `settings_v1` is fatal: a new
//! certificate would silently change this device's fingerprint and break every
//! peer that pinned it.

use std::error::Error;
use std::path::Path;

use handfast_net::{TlsError, TlsIdentity};
use handfast_proto::DeviceId;
use handfast_store::{IdentityRecord, Store};

/// Load the stable device ID, creating and persisting one when absent.
///
/// # Errors
///
/// Fails when the setting cannot be read or written or the stored value is not
/// a valid device ID.
pub fn load_or_create_device_id(store: &Store) -> Result<DeviceId, Box<dyn Error>> {
    if let Some(value) = store.setting("device_id")? {
        return Ok(DeviceId::new(value)?);
    }
    let device_id = DeviceId::generate();
    store.set_setting("device_id", device_id.as_str())?;
    Ok(device_id)
}

/// Load the persisted TLS material or generate and persist it.
///
/// # Errors
///
/// Fails when the record cannot be read or written, when the stored device ID
/// disagrees with `device_id`, or when the stored certificate and key are not a
/// valid pair whose common name is the device ID.
pub fn load_or_generate_tls_identity(
    store: &Store,
    device_id: &DeviceId,
) -> Result<TlsIdentity, Box<dyn Error>> {
    let Some(record) = store.identity()? else {
        let tls = TlsIdentity::generate(device_id.clone())?;
        store.set_identity(&IdentityRecord {
            device_id: tls.device_id().as_str().to_string(),
            cert_der: tls.cert_der().to_vec(),
            key_der: tls.key_der().to_vec(),
        })?;
        return Ok(tls);
    };

    let stored_id = DeviceId::new(record.device_id.clone())?;
    if &stored_id != device_id {
        return Err(format!(
            "stored identity device ID {stored_id} does not match settings_v1 {device_id}"
        )
        .into());
    }
    TlsIdentity::from_parts(stored_id, record.cert_der, record.key_der)
        .map_err(|error: TlsError| format!("stored identity is unusable: {error}").into())
}

/// Restrict the state directory to the owner on Unix.
///
/// # Errors
///
/// Fails when the permissions cannot be read or set.
#[cfg(unix)]
pub fn secure_state_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// The state directory inherits the user profile ACL on Windows.
#[cfg(not(unix))]
pub fn secure_state_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Restrict the state file to the owner on Unix.
///
/// # Errors
///
/// Fails when the permissions cannot be set.
#[cfg(unix)]
pub fn secure_state_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// The state file inherits the user profile ACL on Windows.
#[cfg(not(unix))]
pub fn secure_state_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
