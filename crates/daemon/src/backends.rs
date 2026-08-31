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
#[derive(Debug)]
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
    let mut last_err: Option<String> = None;
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
            // The tool is installed but cannot read this URI (e.g. gio on a
            // KIO-only `kdeconnect://` mount): fall through to the next
            // backend instead of giving up.
            let _ = tokio::fs::remove_file(&tmp).await;
            last_err = Some(format!("{tool} cat failed with {exit}"));
            continue;
        }
        return Ok(Materialized {
            local: tmp.clone(),
            cleanup: Some(tmp),
        });
    }

    Err(Error::Other(format!(
        "cannot read non-local file URI '{}': {}",
        uri,
        match last_err {
            Some(err) => err,
            None => format!(
                "no usable backend found (tried {})",
                if attempted.is_empty() {
                    "none of gio/gvfs-cat/kioclient5 installed".to_string()
                } else {
                    attempted.join(", ")
                }
            ),
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

    let mut last_err: Option<String> = None;
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
        // gio is installed but could not copy into this URI (KIO-only
        // destination): fall through to the KDE backend.
        last_err = Some(format!("gio copy failed with {status}"));
    }
    if command_exists("kioclient5").await {
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
        last_err = Some(format!("kioclient5 copy failed with {status}"));
    }

    Err(Error::Other(format!(
        "could not copy received file into '{}': {}",
        uri,
        last_err.unwrap_or_else(|| "gio/kioclient5 unavailable".to_string())
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
    #![allow(clippy::unwrap_used)] // test helpers assert loudly on failure

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

    // ---------------------------------------------------------------------
    // GVFS/KIO fallback-chain tests. These spawn *fake* `gio`, `gvfs-cat`
    // and `kioclient5` executables staged on PATH, so the chain logic is
    // exercised without a desktop session. The fake tools honour
    // FAKE_<TOOL>_FAIL=1 to simulate an installed-but-unusable backend.
    // ---------------------------------------------------------------------

    static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn unique() -> String {
        format!(
            "{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        )
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Stage fake backend tools and REPLACE PATH with the fake dir, so the
    /// suite is hermetic (the sandbox ships a real `gio`). Fake scripts are
    /// self-contained via absolute paths. Restores PATH and clears FAKE_*
    /// env vars on drop.
    struct FakeTools {
        dir: PathBuf,
        saved_path: String,
    }

    impl FakeTools {
        fn new(names: &[&str]) -> Self {
            let dir = std::env::temp_dir().join(format!("hf-backends-{}", unique()));
            std::fs::create_dir_all(&dir).unwrap();
            let script = r#"#!/bin/sh
[ "$1" = "--version" ] && exit 0
case "$(/usr/bin/basename "$0")" in
  gio) FAIL="$FAKE_GIO_FAIL" ;;
  gvfs-cat) FAIL="$FAKE_GVFS_CAT_FAIL" ;;
  kioclient5) FAIL="$FAKE_KIO_FAIL" ;;
esac
[ -n "$FAIL" ] && { echo "fake $0: forced failure" >&2; exit 1; }
if [ "$1" = "cat" ]; then
  if [ -n "$FAKE_SRC" ]; then /usr/bin/cat "$FAKE_SRC"; else /usr/bin/printf 'content-from-%s' "$(/usr/bin/basename "$0")"; fi
  exit 0
fi
if [ "$1" = "copy" ]; then
  /usr/bin/cp "$2" "$3"; exit $?
fi
exit 0
"#;
            for name in names {
                let path = dir.join(name);
                std::fs::write(&path, script).unwrap();
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let saved_path = std::env::var("PATH").unwrap_or_default();
            // Replace PATH entirely: keeps the sandbox's real gio/kioclient5
            // (if any) from leaking into the tests.
            std::env::set_var("PATH", &dir);
            Self { dir, saved_path }
        }

        fn set(&self, key: &str, value: &str) {
            std::env::set_var(key, value);
        }
    }

    impl Drop for FakeTools {
        fn drop(&mut self) {
            for key in [
                "FAKE_GIO_FAIL",
                "FAKE_GVFS_CAT_FAIL",
                "FAKE_KIO_FAIL",
                "FAKE_SRC",
            ] {
                std::env::remove_var(key);
            }
            std::env::set_var("PATH", &self.saved_path);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn staging_pair() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("hf-backends-dest-{}", unique()));
        let staging_dir = base.join("staging");
        let dest_dir = base.join("dest");
        std::fs::create_dir_all(&staging_dir).unwrap();
        std::fs::create_dir_all(&dest_dir).unwrap();
        let staging = staging_dir.join("stage.bin");
        std::fs::write(&staging, b"payload-bytes").unwrap();
        (staging, dest_dir)
    }

    #[test]
    fn materialize_uri_prefers_gio() {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _fake = FakeTools::new(&["gio", "gvfs-cat", "kioclient5"]);
        let materialized = rt()
            .block_on(materialize_uri("smb://srv/share/f.bin"))
            .unwrap();
        let data = std::fs::read(&materialized.local).unwrap();
        assert_eq!(data, b"content-from-gio");
        assert_eq!(
            materialized.cleanup.as_deref(),
            Some(materialized.local.as_path())
        );
        assert!(materialized.local.exists());
    }

    #[test]
    fn materialize_uri_falls_through_when_gio_fails() {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fake = FakeTools::new(&["gio", "kioclient5"]);
        fake.set("FAKE_GIO_FAIL", "1");
        // gio is installed but cannot read this URI (KIO-only mount): the
        // chain must continue to kioclient5 instead of giving up.
        let materialized = rt()
            .block_on(materialize_uri("kdeconnect://dev/file"))
            .unwrap();
        let data = std::fs::read(&materialized.local).unwrap();
        assert_eq!(data, b"content-from-kioclient5");
    }

    #[test]
    fn materialize_uri_uses_gvfs_cat_when_gio_absent() {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _fake = FakeTools::new(&["gvfs-cat"]);
        let materialized = rt()
            .block_on(materialize_uri("mtp://device/photo.jpg"))
            .unwrap();
        let data = std::fs::read(&materialized.local).unwrap();
        assert_eq!(data, b"content-from-gvfs-cat");
    }

    #[test]
    fn materialize_uri_no_backend_errors() {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _fake = FakeTools::new(&[]);
        let err = rt()
            .block_on(materialize_uri("smb://srv/share/f.bin"))
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no usable backend"), "{msg}");
    }

    #[test]
    fn materialize_uri_reports_last_tool_failure() {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fake = FakeTools::new(&["gio", "gvfs-cat", "kioclient5"]);
        fake.set("FAKE_GIO_FAIL", "1");
        fake.set("FAKE_GVFS_CAT_FAIL", "1");
        fake.set("FAKE_KIO_FAIL", "1");
        let err = rt()
            .block_on(materialize_uri("smb://srv/share/f.bin"))
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("kioclient5 cat failed"), "{msg}");
    }

    #[test]
    fn move_into_uri_copies_via_gio_and_removes_staging() {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _fake = FakeTools::new(&["gio", "kioclient5"]);
        let (staging, dest_dir) = staging_pair();
        rt().block_on(move_into_uri(&staging, &dest_dir.display().to_string()))
            .unwrap();
        assert!(!staging.exists(), "local staging copy must be removed");
        assert_eq!(
            std::fs::read(dest_dir.join("stage.bin")).unwrap(),
            b"payload-bytes"
        );
        let _ = std::fs::remove_dir_all(dest_dir.parent().unwrap());
    }

    #[test]
    fn move_into_uri_falls_back_to_kioclient5_when_gio_copy_fails() {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fake = FakeTools::new(&["gio", "kioclient5"]);
        fake.set("FAKE_GIO_FAIL", "1");
        let (staging, dest_dir) = staging_pair();
        // gio copy fails (KIO-only destination): must fall through to the
        // KDE backend and still succeed.
        rt().block_on(move_into_uri(&staging, &dest_dir.display().to_string()))
            .unwrap();
        assert!(!staging.exists());
        assert_eq!(
            std::fs::read(dest_dir.join("stage.bin")).unwrap(),
            b"payload-bytes"
        );
        let _ = std::fs::remove_dir_all(dest_dir.parent().unwrap());
    }

    #[test]
    fn move_into_uri_no_backend_errors() {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _fake = FakeTools::new(&[]);
        let (staging, dest_dir) = staging_pair();
        let err = rt()
            .block_on(move_into_uri(&staging, &dest_dir.display().to_string()))
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("gio/kioclient5 unavailable"), "{msg}");
        assert!(staging.exists(), "staging must be left in place on failure");
        let _ = std::fs::remove_dir_all(dest_dir.parent().unwrap());
    }

    #[test]
    fn materialize_source_plain_paths_need_no_backend() {
        // No backends staged at all: plain paths and file:// URIs must
        // still work without spawning anything.
        let base = std::env::temp_dir().join(format!("hf-backends-src-{}", unique()));
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("f.txt");
        std::fs::write(&file, b"hi").unwrap();

        let m = rt()
            .block_on(materialize_source(file.to_str().unwrap()))
            .unwrap();
        assert_eq!(m.local, file);
        assert!(m.cleanup.is_none());

        let m = rt()
            .block_on(materialize_source(&format!("file://{}", file.display())))
            .unwrap();
        assert_eq!(m.local, file);
        assert!(m.cleanup.is_none());

        let err = rt()
            .block_on(materialize_source("/no/such/file.bin"))
            .unwrap_err();
        assert!(format!("{err}").contains("file not found"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
