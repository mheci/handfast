//! A vCard address book stored on disk.
//!
//! The desktop client reads contacts from KAddressBook over D-Bus. Handfast
//! reads a directory of `.vcf` files instead: no session-bus dependency, no
//! KDE PIM dependency, and the same file format, so a KDE user can point it at
//! `~/.local/share/contacts` and a Thunderbird user at a directory it exports.
//!
//! The change handshake follows `kdeconnect.contacts`: the phone asks for
//! `(uid, timestamp)` pairs, compares them against what it already has, and
//! requests only the vCards it is missing. Modification time is therefore part
//! of the protocol, not an optimisation, and it is read from the file itself so
//! an external editor that rewrites a card is noticed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::ContactRecord;
use crate::traits::Contacts;

/// Largest accepted `.vcf` file, in bytes.
///
/// A vCard with an embedded photo can be large, but not unbounded: the whole
/// document travels inside one packet.
const MAX_VCARD_BYTES: u64 = 512 * 1024;

/// Longest accepted uid, in bytes.
const MAX_UID_BYTES: usize = 256;

/// An address book backed by a directory of `.vcf` files.
#[derive(Debug, Clone)]
pub struct VcardDirectory {
    root: PathBuf,
}

impl VcardDirectory {
    /// Point the adapter at a directory.
    ///
    /// The directory does not have to exist: an absent address book reports no
    /// contacts, which is the same thing a phone sees from an empty one.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory this adapter reads.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The uid for a file is its name without the `.vcf` extension.
    fn uid_for(path: &Path) -> Option<String> {
        let name = path.file_name()?.to_str()?;
        let stem = name.strip_suffix(".vcf")?;
        if stem.is_empty() || stem.len() > MAX_UID_BYTES || stem.chars().any(char::is_control) {
            return None;
        }
        Some(stem.to_string())
    }

    /// Every readable card, sorted by uid.
    ///
    /// Unreadable or oversized files are skipped rather than failing the whole
    /// listing: one broken card should not hide the other two hundred.
    async fn scan(&self) -> Vec<(String, i64, PathBuf)> {
        let root = self.root.clone();
        let entries = tokio::task::spawn_blocking(move || read_dir(&root))
            .await
            .unwrap_or_default();
        entries
    }
}

fn read_dir(root: &Path) -> Vec<(String, i64, PathBuf)> {
    let Ok(directory) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in directory.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(uid) = VcardDirectory::uid_for(&path) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > MAX_VCARD_BYTES {
            continue;
        }
        let timestamp_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        found.push((uid, timestamp_ms, path));
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found
}

#[async_trait]
impl Contacts for VcardDirectory {
    async fn list(&self) -> Result<Vec<(String, i64)>> {
        Ok(self
            .scan()
            .await
            .into_iter()
            .map(|(uid, timestamp, _)| (uid, timestamp))
            .collect())
    }

    async fn vcards(&self, uids: &[String]) -> Result<Vec<ContactRecord>> {
        let wanted: Vec<String> = uids
            .iter()
            .filter(|uid| !uid.is_empty() && uid.len() <= MAX_UID_BYTES)
            .cloned()
            .collect();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let scanned = self.scan().await;
        let mut records = Vec::new();
        for (uid, timestamp_ms, path) in scanned {
            if !wanted.contains(&uid) {
                continue;
            }
            let Ok(text) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            records.push(ContactRecord {
                uid,
                timestamp_ms,
                vcard: text,
            });
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn book() -> (tempfile::TempDir, VcardDirectory) {
        let temp = tempfile::TempDir::new().unwrap();
        let directory = VcardDirectory::new(temp.path());
        (temp, directory)
    }

    fn write(directory: &Path, name: &str, body: &str) {
        fs::write(directory.join(name), body).unwrap();
    }

    #[tokio::test]
    async fn a_directory_of_cards_is_listed() {
        let (temp, directory) = book();
        write(
            temp.path(),
            "alice.vcf",
            "BEGIN:VCARD\nFN:Alice\nEND:VCARD\n",
        );
        write(temp.path(), "bob.vcf", "BEGIN:VCARD\nFN:Bob\nEND:VCARD\n");

        let list = directory.list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0, "alice");
        assert_eq!(list[1].0, "bob");
        assert!(list[0].1 > 0, "a modification time is reported");
    }

    #[tokio::test]
    async fn only_vcf_files_are_considered() {
        let (temp, directory) = book();
        write(temp.path(), "alice.vcf", "BEGIN:VCARD\nEND:VCARD\n");
        write(temp.path(), "notes.txt", "not a card");
        write(temp.path(), "noextension", "not a card");
        let list = directory.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "alice");
    }

    #[tokio::test]
    async fn an_absent_directory_reports_no_contacts() {
        let directory = VcardDirectory::new("/nonexistent/handfast/addressbook");
        assert!(directory.list().await.unwrap().is_empty());
        assert!(directory.vcards(&["alice".to_string()]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_requested_vcards_are_returned() {
        let (temp, directory) = book();
        write(temp.path(), "alice.vcf", "BEGIN:VCARD\nFN:Alice\nEND:VCARD\n");
        write(temp.path(), "bob.vcf", "BEGIN:VCARD\nFN:Bob\nEND:VCARD\n");

        let records = directory
            .vcards(&["bob".to_string(), "missing".to_string()])
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].uid, "bob");
        assert!(records[0].vcard.contains("FN:Bob"));
        assert!(records[0].timestamp_ms > 0);
    }

    #[tokio::test]
    async fn an_empty_uid_list_short_circuits() {
        let (temp, directory) = book();
        write(temp.path(), "alice.vcf", "BEGIN:VCARD\nEND:VCARD\n");
        assert!(directory.vcards(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_oversized_card_is_skipped() {
        let (temp, directory) = book();
        let big = format!(
            "BEGIN:VCARD\nPHOTO:{}\nEND:VCARD\n",
            "A".repeat(MAX_VCARD_BYTES as usize)
        );
        write(temp.path(), "huge.vcf", &big);
        write(temp.path(), "ok.vcf", "BEGIN:VCARD\nEND:VCARD\n");
        let list = directory.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "ok");
    }

    #[test]
    fn uids_reject_controls_and_empty_names() {
        assert_eq!(VcardDirectory::uid_for(Path::new("/a/.vcf")), None);
        assert_eq!(VcardDirectory::uid_for(Path::new("/a/x.txt")), None);
        assert_eq!(
            VcardDirectory::uid_for(Path::new("/a/good.vcf")),
            Some("good".to_string())
        );
    }
}
