//! File backends: GVFS (GNOME) and KIO (KDE) URI support.
//!
//! Two integration points need to understand more than plain local paths:
//!
//! * **Send sources**: a file manager may hand us a `gvfs://…`, `mtp://…`,
//!   `smb://…`, `sftp://…`, `dav(s)://…` or `kdeconnect://…` URI instead of a
//!   local path. We materialize such URIs into a temporary local file (via
//!   `gio cat`, falling back to `kioclient5 cat` for the KDE stack) before
//!   the payload streams, and clean the temp file up afterwards.
//! * **Receive destinations**: `transfer.save_dir` may itself be a GVFS/KIO
//!   URI (e.g. a mounted Google Drive or Nextcloud folder). The receive
//!   engine always stages locally (rename-based completion is atomic), then
//!   we copy the finished file into the URI with `gio copy` /
//!   `kioclient5 copy` and drop the local staging copy.
//!
//! `file://` URIs and plain paths are handled without spawning anything.

use std::path::{Path, PathBuf};

use handfast_core::error::{Error, Result};
use tokio::io::AsyncWriteExt;
use tracing::debug;

/// Default receive location when nothing is configured.
pub const DEFAULT_SAVE_DIR: &str = "~/Downloads";

/// KV key holding the configured receive destination.
pub const SAVE_DIR_KEY: &str = "transfer.save_dir";

/// A source that was made ready for transfer.
pub struct Materialized {
    /// Path the daemon should open and stream.
    pub local: PathBuf,
    /// Temporary file to remove once the transfer finishes (None for plain
    /// local paths, which the caller owns).
    pub cleanup: Option<PathBuf>,
}

/// Where received files should land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveTarget {
    /// Plain local directory; the engine writes here directly.
    Local(PathBuf),
    /// GVFS/KIO URI; the engine stages locally and we copy into the URI.
    Uri(String),
}

/// Reduce a user-configured destination to a [`SaveTarget`].
#[must_use]
pub fn resolve_save_dir(raw: &str) -> SaveTarget {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("file://") {
        return SaveTarget::Local(expand_tilde(rest));
    }
    if let Some(scheme_end) = trimmed.find("://") {
        let scheme = &trimmed[..scheme_end];
        if scheme != "file" {
            return SaveTarget::Uri(trimmed.to_string());
        }
    }
    SaveTarget::Local(expand_tilde(trimmed))
}

/// Resolve `input` (a path or `file://` URI) to a local path, or materialize
/// a GVFS/KIO URI into a temporary local file.
pub async fn materialize_source(input: &str) -> Result<Materialized> {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix("file://") {
        let path = expand_tilde(rest);
        if !path.is_file() {
            return Err(Error::Other(format!("file not found: {}", path.display())));
        }
        return Ok(Materialized {
            local: path,
            cleanup: None,
        });
    }

    let scheme_end = trimmed.find("://");
    match scheme_end {
        // Plain local path.
        None => {
            let path = expand_tilde(trimmed);
            if !path.is_file() {
                return Err(Error::Other(format!("file not found: {}", path.display())));
            }
            Ok(Materialized {
                local: path,
                cleanup: None,
            })
        }
        Some(_) => materialize_uri(trimmed).await,
    }
}

/// Stream a GVFS/KIO URI into a temp file using `gio` (GNOME) or
/// `kioclient5` (KDE), whichever is available.
async fn materialize_uri(uri: &str) -> Result<Materialized> {
    let tmp = std::env::temp_dir().join(format!(
        "handfast-src-{}-{}",
        std::process::id(),
        random_suffix()
    ));

    let mut attempted: Vec<&str> = Vec::new();
    for tool in ["gio", "gvfs-cat", "kioclient5"] {
        if !command_exists(tool).await {
            continue;
        }
        attempted.push(tool);
        debug!(%tool, %uri, "materializing source URI");
        let mut child = tokio::process::Command::new(tool)
            .arg("cat")
            .arg(uri)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| Error::Other(format!("{tool} spawn failed: {err}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Other(format!("{tool} produced no stdout pipe")))?;
        let mut out = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .await?;
        let mut reader = tokio::io::BufReader::new(stdout);
        tokio::io::copy(&mut reader, &mut out).await?;
        out.flush().await?;
        let exit = child.wait().await?;
        if !exit.success() {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Error::Other(format!("{tool} cat failed with {exit}")));
        }
        return Ok(Materialized {
            local: tmp.clone(),
            cleanup: Some(tmp),
        });
    }

    Err(Error::Other(format!(
        "cannot read non-local file URI '{}': no usable backend found (tried {})",
        uri,
        if attempted.is_empty() {
            "none of gio/gvfs-cat/kioclient5 installed".to_string()
        } else {
            attempted.join(", ")
        }
    )))
}

/// Copy a finished local file into a GVFS/KIO URI destination, then remove
/// the local copy. `file_name` is the basename the destination directory
/// should keep.
pub async fn move_into_uri(local: &Path, uri: &str) -> Result<()> {
    let file_name = local
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Other("local staging file has no name".into()))?;
    let destination = format!("{}/{}", uri.trim_end_matches('/'), file_name);

    if command_exists("gio").await {
        debug!(%destination, "copying into GVFS destination");
        let status = tokio::process::Command::new("gio")
            .arg("copy")
            .arg(local)
            .arg(&destination)
            .status()
            .await?;
        if status.success() {
            let _ = tokio::fs::remove_file(local).await;
            return Ok(());
        }
    } else if command_exists("kioclient5").await {
        debug!(%destination, "copying into KIO destination");
        let status = tokio::process::Command::new("kioclient5")
            .arg("copy")
            .arg(local)
            .arg(&destination)
            .status()
            .await?;
        if status.success() {
            let _ = tokio::fs::remove_file(local).await;
            return Ok(());
        }
    }

    Err(Error::Other(format!(
        "could not copy received file into '{}' (gio/kioclient5 unavailable or failed)",
        uri
    )))
}

/// Expand a leading `~` to the current user's home directory.
#[must_use]
pub fn expand_tilde(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

/// Whether `tool` is on PATH.
async fn command_exists(tool: &str) -> bool {
    tokio::process::Command::new(tool)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Uniquifying suffix for temp files (pid + a cheap counter is enough for
/// concurrent sends from one daemon).
fn random_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{:x}",
        COUNTER.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_dir_resolution() {
        assert_eq!(
            resolve_save_dir("~/Downloads"),
            SaveTarget::Local(expand_tilde("~/Downloads"))
        );
        assert_eq!(
            resolve_save_dir("file:///home/u/x"),
            SaveTarget::Local(PathBuf::from("/home/u/x"))
        );
        assert_eq!(
            resolve_save_dir("gvfs:///mount/drive"),
            SaveTarget::Uri("gvfs:///mount/drive".into())
        );
        assert_eq!(
            resolve_save_dir("smb://server/share"),
            SaveTarget::Uri("smb://server/share".into())
        );
    }

    #[test]
    fn tilde_expansion() {
        let home = std::env::var("HOME").expect("HOME set in tests");
        assert_eq!(expand_tilde("~/x"), PathBuf::from(&home).join("x"));
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }
}
