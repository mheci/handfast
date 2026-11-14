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
    TlsIdentity::from_parts(stored_id, record.cert_der.clone(), record.key_der.clone())
        .map_err(|error: TlsError| format!("stored identity is unusable: {error}").into())
}

/// Reject a state path that is itself a symlink or reparse point, or is owned
/// by another user.
///
/// A missing path passes, because the daemon creates it itself. Run the check
/// before creating or opening the path: `create_dir_all` and the store follow a
/// link, so the write would land in a file the current user does not own.
///
/// # Errors
///
/// Fails when the path exists and is a symlink or Windows reparse point, when
/// its owner is not the current user, or when its metadata cannot be read.
pub fn ensure_owned_state_path(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("state path {} is a symlink", path.display()),
            ));
        }
        let owner = metadata.uid();
        let current = effective_uid();
        if owner != current {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "state path {} is owned by uid {owner}, expected {current}",
                    path.display()
                ),
            ));
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("state path {} is a reparse point", path.display()),
            ));
        }
        if !handfast_desktop::winacl::is_owned_by_current_user(path)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "state path {} is not owned by the current user",
                    path.display()
                ),
            ));
        }
    }

    #[cfg(not(any(unix, windows)))]
    let _ = metadata;

    Ok(())
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions, reads no memory, and cannot fail.
    unsafe { libc::geteuid() }
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

/// Replace the state directory's inherited ACL with a protected DACL for the
/// current user on Windows.
///
/// # Errors
///
/// Fails when the DACL cannot be applied. A failure is fatal: the private key
/// lives under this directory.
#[cfg(windows)]
pub fn secure_state_dir(path: &Path) -> std::io::Result<()> {
    handfast_desktop::winacl::protect_current_user(path)
}

/// The state directory keeps its inherited ACL on other non-Unix targets.
#[cfg(not(any(unix, windows)))]
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

/// Replace the state file's inherited ACL with a protected DACL for the
/// current user on Windows.
///
/// # Errors
///
/// Fails when the DACL cannot be applied. A failure is fatal: this file holds
/// the TLS private key.
#[cfg(windows)]
pub fn secure_state_file(path: &Path) -> std::io::Result<()> {
    handfast_desktop::winacl::protect_current_user(path)
}

/// The state file keeps its inherited ACL on other non-Unix targets.
#[cfg(not(any(unix, windows)))]
pub fn secure_state_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_state_paths_pass() {
        let dir = tempfile::tempdir().unwrap();
        ensure_owned_state_path(&dir.path().join("data")).unwrap();
        ensure_owned_state_path(&dir.path().join("data").join("state.db")).unwrap();
    }

    #[test]
    fn owned_state_dir_and_file_pass() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir(&data_dir).unwrap();
        ensure_owned_state_path(&data_dir).unwrap();

        let state_path = data_dir.join("state.db");
        std::fs::write(&state_path, b"state").unwrap();
        ensure_owned_state_path(&state_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_dir_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("data");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = ensure_owned_state_path(&link).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.db");
        std::fs::write(&target, b"state").unwrap();
        let link = dir.path().join("state.db");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = ensure_owned_state_path(&link).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    // A junction is the reparse point an unprivileged Windows process can
    // create; a directory symlink needs a privilege a normal user lacks.
    #[cfg(windows)]
    #[test]
    fn windows_reparse_point_is_rejected() {
        use std::os::windows::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("data");
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .raw_arg(format!(
                "mklink /J \"{}\" \"{}\"",
                link.display(),
                target.display()
            ))
            .status()
            .unwrap();
        assert!(status.success(), "create a directory junction");

        let error = ensure_owned_state_path(&link).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(windows)]
    #[test]
    fn windows_state_paths_are_private_to_the_current_user() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir(&data_dir).unwrap();

        secure_state_dir(&data_dir).unwrap();
        assert!(handfast_desktop::winacl::is_private_to_current_user(&data_dir).unwrap());

        let state_path = data_dir.join("state.db");
        std::fs::write(&state_path, b"state").unwrap();
        secure_state_file(&state_path).unwrap();
        assert!(handfast_desktop::winacl::is_private_to_current_user(&state_path).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_state_acl_application_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir(&data_dir).unwrap();
        let state_path = data_dir.join("state.db");
        std::fs::write(&state_path, b"state").unwrap();

        for _ in 0..2 {
            secure_state_dir(&data_dir).unwrap();
            secure_state_file(&state_path).unwrap();
        }
        assert!(handfast_desktop::winacl::is_private_to_current_user(&data_dir).unwrap());
        assert!(handfast_desktop::winacl::is_private_to_current_user(&state_path).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_state_acl_failure_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(secure_state_dir(&dir.path().join("missing")).is_err());
        assert!(secure_state_file(&dir.path().join("missing.db")).is_err());
    }
}
