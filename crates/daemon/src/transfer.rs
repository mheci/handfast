//! Inbound file-transfer staging.
//!
//! [`TransferEngine`] owns the *receive* side of the protocol: control-plane
//! metadata ([`TransferMeta`]) opens a `.handfast-part` staging file inside
//! the save directory, streamed chunks append to it, and completion atomically
//! renames the staging file to its final name. Peer-supplied file names are
//! never trusted — [`sanitize_file_name`] reduces them to a bare component so
//! a hostile sender can never escape the save directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use handfast_core::error::{Error, Result};
use handfast_protocol::transfer::{TransferMeta, CHUNK_SIZE};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info};

/// Suffix of the staging file while a download is still incomplete.
const PART_SUFFIX: &str = ".handfast-part";

/// Fallback name when a peer sends something with no usable file name.
const FALLBACK_NAME: &str = "received-file";

/// Maximum length (in chars) of a sanitized file name kept on disk.
const MAX_FILE_NAME_LEN: usize = 240;

/// How many numbered copies (`name (n).ext`) are tried before giving up on
/// finding a free destination slot for a colliding file name.
const MAX_NAME_COLLISIONS: u32 = 10_000;

/// Bookkeeping for one in-flight inbound transfer.
#[derive(Debug)]
struct IncomingTransfer {
    meta: TransferMeta,
    /// Sanitized bare file name used for the final destination.
    final_name: String,
    /// Staging file inside the save directory.
    part_path: PathBuf,
    /// Bytes appended so far; equals the staging file's length.
    received: u64,
}

/// Manages file receive operations into a single save directory.
///
/// One engine per daemon is expected; methods take `&mut self`, which keeps
/// chunk ordering per transfer trivially correct when driven from an actor
/// loop like [`crate::device::Manager`].
#[derive(Debug)]
pub struct TransferEngine {
    save_dir: PathBuf,
    incoming: HashMap<String, IncomingTransfer>,
}

impl TransferEngine {
    /// Create an engine that stages and stores downloads under `save_dir`.
    ///
    /// The directory itself is created lazily by [`Self::start_receive`].
    #[must_use]
    pub fn new(save_dir: PathBuf) -> Self {
        Self {
            save_dir,
            incoming: HashMap::new(),
        }
    }

    /// Begin receiving `meta`: create the `.handfast-part` staging file.
    ///
    /// The peer-provided `file_name` is sanitized before any path is built,
    /// and the staging file is created exclusively so two concurrent
    /// transfers cannot clobber each other's bytes.
    pub async fn start_receive(&mut self, meta: TransferMeta) -> Result<()> {
        if self.incoming.contains_key(&meta.transfer_id) {
            return Err(Error::Other(format!(
                "transfer '{}' already active",
                meta.transfer_id
            )));
        }
        tokio::fs::create_dir_all(&self.save_dir).await?;
        let final_name = sanitize_file_name(&meta.file_name);
        let part_path = self.save_dir.join(format!("{final_name}{PART_SUFFIX}"));
        // Reserve the name atomically; the handle closes immediately, chunks
        // are appended through fresh handles in [`Self::write_chunk`].
        drop(
            tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&part_path)
                .await?,
        );
        info!(
            id = %meta.transfer_id,
            device = %meta.device_id,
            name = %final_name,
            size = meta.file_size,
            "receiving file"
        );
        self.incoming.insert(
            meta.transfer_id.clone(),
            IncomingTransfer {
                meta,
                part_path,
                final_name,
                received: 0,
            },
        );
        Ok(())
    }

    /// Append `data` to the staging file of `transfer_id`.
    ///
    /// Returns the number of bytes received so far. Chunks larger than
    /// [`CHUNK_SIZE`] or pushes past the declared `file_size` are rejected as
    /// protocol violations; unknown ids are errors too.
    pub async fn write_chunk(&mut self, transfer_id: &str, data: &[u8]) -> Result<u64> {
        if data.len() > CHUNK_SIZE {
            return Err(Error::Other(format!(
                "chunk of {} bytes exceeds CHUNK_SIZE ({CHUNK_SIZE})",
                data.len()
            )));
        }
        let Some(transfer) = self.incoming.get_mut(transfer_id) else {
            return Err(Error::Other(format!("unknown transfer '{transfer_id}'")));
        };
        let total_after = transfer.received + data.len() as u64;
        if total_after > transfer.meta.file_size {
            return Err(Error::Other(format!(
                "transfer '{transfer_id}' overflows declared size: {} + {} > {}",
                transfer.received,
                data.len(),
                transfer.meta.file_size
            )));
        }
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&transfer.part_path)
            .await?;
        file.write_all(data).await?;
        file.flush().await?;
        transfer.received = total_after;
        Ok(total_after)
    }

    /// Finalize `transfer_id`: rename its staging file to the final name.
    ///
    /// The transfer must have received exactly the declared size. When a file
    /// of that name already exists, a numbered copy (`name (1).ext`) is used
    /// instead so completed downloads are never overwritten. Returns the
    /// destination path.
    pub async fn finish_receive(&mut self, transfer_id: &str) -> Result<PathBuf> {
        let Some(transfer) = self.incoming.get(transfer_id) else {
            return Err(Error::Other(format!("unknown transfer '{transfer_id}'")));
        };
        if transfer.received != transfer.meta.file_size {
            return Err(Error::Other(format!(
                "transfer '{transfer_id}' incomplete: {}/{} bytes",
                transfer.received, transfer.meta.file_size
            )));
        }
        let part_path = transfer.part_path.clone();
        let final_name = transfer.final_name.clone();
        let destination = free_destination(&self.save_dir, &final_name).await;
        tokio::fs::rename(&part_path, &destination).await?;
        self.incoming.remove(transfer_id);
        info!(id = %transfer_id, to = %destination.display(), "file received");
        Ok(destination)
    }

    /// Cancel `transfer_id` and delete its partial staging file.
    pub async fn abort(&mut self, transfer_id: &str) -> Result<()> {
        let Some(transfer) = self.incoming.remove(transfer_id) else {
            return Err(Error::Other(format!("unknown transfer '{transfer_id}'")));
        };
        match tokio::fs::remove_file(&transfer.part_path).await {
            Ok(()) => {}
            // Already gone (e.g. crash recovery swept it): still cancelled.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        debug!(device = %transfer.meta.device_id, id = %transfer_id, "transfer aborted");
        Ok(())
    }

    /// Whether a receive with this id is currently in flight.
    #[must_use]
    pub fn is_active(&self, id: &str) -> bool {
        self.incoming.contains_key(id)
    }

    /// Number of receives currently in flight.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.incoming.len()
    }
}

/// Pick a not-yet-taken path inside `dir` for `file_name`.
///
/// Prefers the plain name; on collision falls back to `stem (n).ext`
/// numbering, mirroring what desktop browsers do for duplicate downloads.
async fn free_destination(dir: &Path, file_name: &str) -> PathBuf {
    let first = dir.join(file_name);
    if !exists(&first).await {
        return first;
    }
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(FALLBACK_NAME);
    let extension = Path::new(file_name).extension().and_then(|s| s.to_str());
    for n in 1..MAX_NAME_COLLISIONS {
        let candidate = match extension {
            Some(ext) => dir.join(format!("{stem} ({n}).{ext}")),
            None => dir.join(format!("{stem} ({n})")),
        };
        if !exists(&candidate).await {
            return candidate;
        }
    }
    first
}

/// `tokio::fs::try_exists` flattened to a bool; IO trouble reads as "taken".
async fn exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(true)
}

/// Reduce a peer-supplied file name to a safe bare component.
///
/// Directory components (both separators), `.`/`..` traversal hops, Windows-
/// hostile characters and control bytes are stripped; an unusable result
/// collapses to [`FALLBACK_NAME`]. The output never contains `/` or `\`,
/// so joining it onto the save directory cannot traverse upwards.
#[must_use]
fn sanitize_file_name(raw: &str) -> String {
    let mut picked: Option<String> = None;
    for segment in raw.split(['/', '\\']) {
        match segment {
            "" | "." | ".." => {}
            usable => picked = Some(usable.to_string()),
        }
    }
    let base = picked.unwrap_or_else(|| FALLBACK_NAME.to_string());
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 || c == '\u{7f}' => '_',
            c => c,
        })
        .collect();
    // Trailing dots/spaces are invisible on Windows and invite confusion.
    let trimmed = cleaned.trim_end_matches(['.', ' ']);
    let truncated: String = trimmed.chars().take(MAX_FILE_NAME_LEN).collect();
    if truncated.is_empty() {
        FALLBACK_NAME.to_string()
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, file_name: &str, file_size: u64) -> TransferMeta {
        TransferMeta {
            transfer_id: id.to_string(),
            device_id: "peer-device".to_string(),
            file_name: file_name.to_string(),
            file_size,
        }
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "handfastd-transfer-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: &Path) {
        let _removed = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sanitizer_strips_traversal_and_separators() {
        assert_eq!(sanitize_file_name("report.pdf"), "report.pdf");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("..\\..\\win\\evil.txt"), "evil.txt");
        assert_eq!(sanitize_file_name("/absolute/path.bin"), "path.bin");
        assert_eq!(sanitize_file_name("a/../b.txt"), "b.txt");
        assert_eq!(sanitize_file_name("."), FALLBACK_NAME);
        assert_eq!(sanitize_file_name(".."), FALLBACK_NAME);
        assert_eq!(sanitize_file_name(""), FALLBACK_NAME);
        assert_eq!(sanitize_file_name("...."), FALLBACK_NAME);
        // Windows-hostile characters are replaced, never dropped silently.
        assert_eq!(sanitize_file_name("a:b*c?.txt"), "a_b_c_.txt");
        let sanitized = sanitize_file_name("../".repeat(64) + &"x".repeat(MAX_FILE_NAME_LEN * 2));
        assert!(!sanitized.contains('/'));
        assert!(sanitized.chars().count() <= MAX_FILE_NAME_LEN);
    }

    #[tokio::test]
    async fn full_lifecycle_writes_then_finalizes() {
        let dir = unique_dir("lifecycle");
        let mut engine = TransferEngine::new(dir.clone());
        let payload = vec![7u8; CHUNK_SIZE + 11];

        engine
            .start_receive(meta("t1", "photo.jpg", payload.len() as u64))
            .await
            .unwrap();
        assert!(engine.is_active("t1"));
        assert_eq!(engine.active_count(), 1);
        assert!(dir.join("photo.jpg.handfast-part").is_file());

        let done = engine
            .write_chunk("t1", &payload[..CHUNK_SIZE])
            .await
            .unwrap();
        assert_eq!(done, CHUNK_SIZE as u64);
        let done = engine
            .write_chunk("t1", &payload[CHUNK_SIZE..])
            .await
            .unwrap();
        assert_eq!(done, payload.len() as u64);

        let final_path = engine.finish_receive("t1").await.unwrap();
        assert_eq!(final_path, dir.join("photo.jpg"));
        assert!(!engine.is_active("t1"));
        assert_eq!(engine.active_count(), 0);
        assert!(!dir.join("photo.jpg.handfast-part").exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), payload);
        cleanup(&dir);
    }

    #[tokio::test]
    async fn hostile_names_cannot_escape_save_dir() {
        let dir = unique_dir("traversal");
        let parent = dir.parent().unwrap().to_path_buf();
        let mut engine = TransferEngine::new(dir.clone());

        let evils = [
            "../../escaped.txt",
            "..\\..\\win_escaped.txt",
            "/absolute/path.txt",
            "sub/dir/nested.bin",
            "....",
        ];
        for (i, evil) in evils.iter().enumerate() {
            let id = format!("e{i}");
            engine.start_receive(meta(&id, evil, 2)).await.unwrap();
            engine.write_chunk(&id, b"ok").await.unwrap();
            let landed = engine.finish_receive(&id).await.unwrap();
            assert_eq!(landed.parent().unwrap(), dir, "name {evil:?} escaped");
        }

        // Every payload landed flat inside the save dir under a clean name...
        assert!(dir.join("escaped.txt").is_file());
        assert!(dir.join("win_escaped.txt").is_file());
        assert!(dir.join("path.txt").is_file());
        assert!(dir.join("nested.bin").is_file());
        assert!(dir.join("received-file").is_file()); // "...." collapsed
                                                      // ...and nothing leaked next to the save dir.
        let leaked = std::fs::read_dir(&parent)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("escape"));
        assert!(!leaked);
        cleanup(&dir);
    }

    #[tokio::test]
    async fn abort_removes_partial_state_and_rejects_followups() {
        let dir = unique_dir("abort");
        let mut engine = TransferEngine::new(dir.clone());
        engine
            .start_receive(meta("a1", "movie.mkv", 100))
            .await
            .unwrap();
        engine.write_chunk("a1", &[0u8; 40]).await.unwrap();
        let part = dir.join("movie.mkv.handfast-part");
        assert!(part.is_file());

        engine.abort("a1").await.unwrap();
        assert!(!part.exists());
        assert!(!engine.is_active("a1"));
        assert_eq!(engine.active_count(), 0);

        // A dead transfer accepts no further traffic.
        assert!(engine.abort("a1").await.is_err());
        assert!(engine.write_chunk("a1", &[0u8]).await.is_err());
        assert!(engine.finish_receive("a1").await.is_err());
        cleanup(&dir);
    }

    #[tokio::test]
    async fn interleaved_transfers_stay_isolated() {
        let dir = unique_dir("concurrent");
        let mut engine = TransferEngine::new(dir.clone());
        let red = vec![1u8; 3 * CHUNK_SIZE];
        let blue = vec![2u8; CHUNK_SIZE];
        engine
            .start_receive(meta("red", "red.bin", red.len() as u64))
            .await
            .unwrap();
        engine
            .start_receive(meta("blue", "blue.bin", blue.len() as u64))
            .await
            .unwrap();
        assert_eq!(engine.active_count(), 2);

        let red_done_1 = engine.write_chunk("red", &red[..CHUNK_SIZE]).await.unwrap();
        let blue_done = engine.write_chunk("blue", &blue).await.unwrap();
        assert_eq!(red_done_1, CHUNK_SIZE as u64);
        assert_eq!(blue_done, CHUNK_SIZE as u64);

        let blue_path = engine.finish_receive("blue").await.unwrap();
        assert!(!engine.is_active("blue"));
        // Finishing blue left red untouched and still mid-flight.
        assert!(engine.is_active("red"));
        assert!(dir.join("red.bin.handfast-part").is_file());

        engine
            .write_chunk("red", &red[CHUNK_SIZE..2 * CHUNK_SIZE])
            .await
            .unwrap();
        engine
            .write_chunk("red", &red[2 * CHUNK_SIZE..])
            .await
            .unwrap();
        let red_path = engine.finish_receive("red").await.unwrap();

        assert_eq!(std::fs::read(&blue_path).unwrap(), blue);
        assert_eq!(std::fs::read(&red_path).unwrap(), red);
        assert_eq!(engine.active_count(), 0);
        cleanup(&dir);
    }

    #[tokio::test]
    async fn protocol_violations_are_rejected() {
        let dir = unique_dir("violations");
        let mut engine = TransferEngine::new(dir.clone());

        // Duplicate id.
        engine
            .start_receive(meta("dup", "same.txt", 0))
            .await
            .unwrap();
        assert!(engine
            .start_receive(meta("dup", "same.txt", 0))
            .await
            .is_err());

        // Oversize chunk (protocol frames at CHUNK_SIZE).
        engine
            .start_receive(meta("big", "big.bin", 4))
            .await
            .unwrap();
        assert!(engine.write_chunk("big", &[0u8; CHUNK_SIZE]).await.is_err());
        // Overflowing the declared size.
        assert!(engine.write_chunk("big", &[0u8; 5]).await.is_err());
        // Incomplete finish leaves the transfer alive for retry/abort.
        engine.write_chunk("big", &[0u8; 2]).await.unwrap();
        assert!(matches!(
            engine.finish_receive("big").await,
            Err(Error::Other(_))
        ));
        assert!(engine.is_active("big"));

        // Unknown ids error everywhere without touching state.
        assert!(engine.write_chunk("ghost", b"").await.is_err());
        assert!(engine.finish_receive("ghost").await.is_err());
        assert!(engine.abort("ghost").await.is_err());
        assert_eq!(engine.active_count(), 2);

        engine.abort("dup").await.unwrap();
        engine.abort("big").await.unwrap();
        assert_eq!(engine.active_count(), 0);
        cleanup(&dir);
    }

    #[tokio::test]
    async fn colliding_names_get_numbered_copies() {
        let dir = unique_dir("collision");
        let mut engine = TransferEngine::new(dir.clone());
        for i in 0..3 {
            let id = format!("c{i}");
            engine
                .start_receive(meta(&id, "notes.txt", 4))
                .await
                .unwrap();
            engine.write_chunk(&id, b"data").await.unwrap();
            engine.finish_receive(&id).await.unwrap();
        }
        assert!(dir.join("notes.txt").is_file());
        assert!(dir.join("notes (1).txt").is_file());
        assert!(dir.join("notes (2).txt").is_file());
        assert_eq!(std::fs::read(dir.join("notes (2).txt")).unwrap(), b"data");
        cleanup(&dir);
    }
}
