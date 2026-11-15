//! The update's signature check on Unix.
//!
//! There is none. Linux integrity is TLS to an allowed host plus the SHA-256
//! from the same release, which is the residual risk the design names.

use std::path::Path;

use super::UpdateError;

/// Accept the download. The release carries no Linux signature to check.
pub(super) fn verify(_path: &Path) -> Result<(), UpdateError> {
    Ok(())
}
