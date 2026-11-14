//! Ownership checks for paths the daemon writes into.

use std::io;
use std::path::Path;

/// Whether `path` exists and is owned by the current user.
///
/// # Errors
///
/// Fails when the path metadata cannot be read, including when it is missing.
pub fn is_owned_by_current_user(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        Ok(std::fs::symlink_metadata(path)?.uid() == effective_uid())
    }

    #[cfg(windows)]
    {
        crate::winacl::is_owned_by_current_user(path)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(true)
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions, reads no memory, and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_created_by_this_process_are_owned_by_the_current_user() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_owned_by_current_user(dir.path()).unwrap());

        let file = dir.path().join("file.txt");
        std::fs::write(&file, b"data").unwrap();
        assert!(is_owned_by_current_user(&file).unwrap());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_owned_by_current_user(&dir.path().join("missing")).is_err());
    }
}
