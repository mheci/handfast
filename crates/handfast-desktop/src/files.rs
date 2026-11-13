//! Incoming and outgoing file handling for share transfers.
//!
//! [`LocalFiles`] is the only writer of a received path. It treats the peer
//! filename as an untrusted basename, writes into a private temp directory and
//! commits with a no-clobber operation.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use thiserror::Error;

const TEMP_DIR_NAME: &str = ".handfast-tmp";
const FALLBACK_NAME: &str = "shared-file";
const MAX_DUPLICATE_SUFFIX: usize = 99;
const MIN_NAME_BUDGET: usize = 8;
const SUFFIX_RESERVE_STEP: usize = 4;
const TEMP_NAME_ATTEMPTS: usize = 8;

/// A request to receive one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingRequest {
    /// Peer-supplied file name. Treated as an untrusted basename.
    pub filename: Option<String>,
    /// Declared payload size. `None` means the peer did not declare one.
    pub size: Option<u64>,
    /// Peer asked to open the file after saving. Parsed but not acted on.
    pub open: bool,
}

/// An open temp file that becomes the final file on [`commit`](IncomingFile::commit).
pub struct IncomingFile {
    root: PathBuf,
    temp_path: PathBuf,
    name: String,
    limits: FileLimits,
    file: Option<tokio::fs::File>,
    #[cfg(test)]
    strategy: CommitStrategy,
}

impl IncomingFile {
    /// Borrow the writer for the payload stream.
    pub fn writer(&mut self) -> &mut tokio::fs::File {
        match self.file.as_mut() {
            Some(file) => file,
            None => unreachable!("the temp handle is open until commit or abort"),
        }
    }

    /// Sync the temp file and commit it under a free name in the root.
    ///
    /// The commit never replaces an existing file. A hard link reserves the
    /// name atomically; filesystems that reject hard links get a `create_new`
    /// plus copy fallback. A duplicate name gets ` (1)`, ` (2)` and so on.
    ///
    /// # Errors
    /// Fails when the sync, the name allocation or the commit itself fails.
    /// The temp file is removed when this value drops.
    pub async fn commit(mut self) -> Result<PathBuf, FilesError> {
        let Some(file) = self.file.take() else {
            return Err(FilesError::NoHandle);
        };
        file.sync_all().await?;
        drop(file);

        #[cfg(test)]
        let force_copy = self.strategy == CommitStrategy::Copy;
        #[cfg(not(test))]
        let force_copy = false;

        let mut budget = self
            .limits
            .max_name_bytes
            .saturating_sub(self.limits.suffix_reserve);
        let mut attempt = 0;
        loop {
            let candidate = candidate_name(&self.name, attempt, budget);
            let destination = self.root.join(&candidate);
            match try_commit(&self.temp_path, &destination, force_copy).await {
                Ok(()) => {
                    remove_temp(&self.temp_path).await;
                    sync_root(&self.root).await;
                    return Ok(destination);
                }
                Err(CommitError::Exists) => {
                    if attempt >= MAX_DUPLICATE_SUFFIX {
                        return Err(FilesError::DuplicateName(self.name.clone()));
                    }
                    attempt += 1;
                }
                Err(CommitError::NameTooLong) => {
                    if budget <= MIN_NAME_BUDGET {
                        return Err(FilesError::NameTooLong(self.name.clone()));
                    }
                    budget = budget.saturating_sub(SUFFIX_RESERVE_STEP);
                }
                Err(CommitError::Io(error)) => return Err(FilesError::Io(error)),
            }
        }
    }

    /// Close and delete the temp file. Idempotent.
    pub async fn abort(mut self) {
        drop(self.file.take());
        remove_temp(&self.temp_path).await;
    }
}

impl Drop for IncomingFile {
    fn drop(&mut self) {
        drop(self.file.take());
        // A committed file already removed the temp, so NotFound is expected.
        let _ = std::fs::remove_file(&self.temp_path);
    }
}

impl std::fmt::Debug for IncomingFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncomingFile")
            .field("root", &self.root)
            .field("temp_path", &self.temp_path)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A readable source file for an outbound share.
#[derive(Debug)]
pub struct OutgoingFile {
    /// Base name sent to the peer.
    pub filename: String,
    /// File size in bytes.
    pub size: u64,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_ms: Option<i64>,
    /// Last modification time in milliseconds since the Unix epoch.
    pub modified_ms: Option<i64>,
    /// File contents.
    pub reader: tokio::fs::File,
}

/// Incoming and outgoing file operations.
#[async_trait]
pub trait Files: Send + Sync {
    /// Open a temp file for one incoming transfer.
    ///
    /// # Errors
    /// Fails when the declared size exceeds the limit, free space is short, or
    /// the private temp directory is unusable.
    async fn open_incoming(&self, request: IncomingRequest) -> Result<IncomingFile, FilesError>;

    /// Open a regular file for an outbound transfer.
    ///
    /// # Errors
    /// Fails for a directory, a special file or an unreadable file.
    async fn open_outgoing(&self, path: &Path) -> Result<OutgoingFile, FilesError>;

    /// Remove stale temp files older than `max_age`.
    ///
    /// # Errors
    /// Fails when the private temp directory cannot be read.
    async fn sweep_stale(&self, max_age: Duration) -> Result<SweepReport, FilesError>;
}

/// Limits for incoming and outgoing files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileLimits {
    /// Largest declared file size in bytes. Defaults to 2 GiB.
    pub max_file_size: u64,
    /// Extra bytes allowed beyond the declared size. Defaults to 64 MiB.
    pub max_extra: u64,
    /// Free space kept unused on the incoming filesystem. Defaults to 64 MiB.
    pub min_free_space: u64,
    /// Largest file name component in bytes. Defaults to 255.
    pub max_name_bytes: usize,
    /// Bytes reserved for a duplicate suffix such as ` (99)`. Defaults to 16.
    pub suffix_reserve: usize,
}

impl Default for FileLimits {
    fn default() -> Self {
        Self {
            max_file_size: 2 * 1024 * 1024 * 1024,
            max_extra: 64 * 1024 * 1024,
            min_free_space: 64 * 1024 * 1024,
            max_name_bytes: 255,
            suffix_reserve: 16,
        }
    }
}

/// Outcome of a stale temp sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepReport {
    /// Number of temp files removed.
    pub removed: usize,
}

/// Errors returned by the files adapter.
#[derive(Debug, Error)]
pub enum FilesError {
    /// A filesystem operation failed.
    #[error("file io: {0}")]
    Io(#[from] io::Error),

    /// The declared file is larger than the configured cap.
    #[error("file size {size} exceeds the {limit} byte limit")]
    TooLarge {
        /// Declared size.
        size: u64,
        /// Configured cap.
        limit: u64,
    },

    /// The incoming filesystem does not have the required free space.
    #[error("not enough free space: need {needed} bytes, have {available}")]
    NoSpace {
        /// Bytes the adapter reserves.
        needed: u64,
        /// Bytes currently available.
        available: u64,
    },

    /// The incoming root exists but is not a directory.
    #[error("incoming root is not a directory: {0}")]
    RootNotDirectory(PathBuf),

    /// The incoming root is a symlink.
    #[error("incoming root is a symlink: {0}")]
    RootSymlink(PathBuf),

    /// The private temp path exists but is not a directory.
    #[error("private temp path is not a directory: {0}")]
    TempNotDirectory(PathBuf),

    /// The private temp path is a symlink.
    #[error("private temp path is a symlink: {0}")]
    TempSymlink(PathBuf),

    /// The private temp directory is accessible by other users.
    #[error("private temp directory is accessible by other users: {0}")]
    TempPermissions(PathBuf),

    /// No candidate name fits the filesystem limit.
    #[error("file name is too long: {0}")]
    NameTooLong(String),

    /// Every duplicate candidate up to ` (99)` already exists.
    #[error("too many files with the name: {0}")]
    DuplicateName(String),

    /// The outgoing path is not a regular file.
    #[error("outgoing path is not a regular file: {0}")]
    NotRegularFile(PathBuf),

    /// The outgoing path could not be opened for reading.
    #[error("outgoing path is not readable: {0}")]
    Unreadable(PathBuf),

    /// The incoming temp file handle was already taken.
    #[error("incoming file handle is closed")]
    NoHandle,
}

/// A files adapter that writes into a local incoming root.
#[derive(Debug)]
pub struct LocalFiles {
    root: PathBuf,
    temp_dir: PathBuf,
    limits: FileLimits,
    #[cfg(test)]
    strategy: CommitStrategy,
}

impl LocalFiles {
    /// Create the incoming root and its private temp directory.
    ///
    /// An existing root keeps its permissions. A new root and the temp
    /// directory are created with mode 0700 on Unix.
    ///
    /// # Errors
    /// Fails when the root or temp path exists as a file or symlink, when an
    /// existing temp directory is group or world accessible on Unix, or when a
    /// directory cannot be created.
    pub fn new(root: impl AsRef<Path>, limits: FileLimits) -> Result<Self, FilesError> {
        let root = root.as_ref().to_path_buf();
        ensure_root(&root)?;
        let temp_dir = root.join(TEMP_DIR_NAME);
        ensure_temp_dir(&temp_dir)?;
        Ok(Self {
            root,
            temp_dir,
            limits,
            #[cfg(test)]
            strategy: CommitStrategy::HardLink,
        })
    }

    /// Incoming root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Private temp directory.
    #[must_use]
    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }

    #[cfg(test)]
    fn with_strategy(
        root: impl AsRef<Path>,
        limits: FileLimits,
        strategy: CommitStrategy,
    ) -> Result<Self, FilesError> {
        let mut files = Self::new(root, limits)?;
        files.strategy = strategy;
        Ok(files)
    }

    async fn create_temp(&self) -> Result<(PathBuf, tokio::fs::File), FilesError> {
        for _ in 0..TEMP_NAME_ATTEMPTS {
            let path = self.temp_dir.join(format!("{}.part", uuid::Uuid::new_v4()));
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&path).await {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(FilesError::Io(error)),
            }
        }
        Err(FilesError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a temp file name",
        )))
    }

    fn reserve_space(&self, size: Option<u64>) -> Result<(), FilesError> {
        let available = fs4::available_space(&self.temp_dir)?;
        let needed = match size {
            Some(declared) => declared
                .checked_add(self.limits.max_extra)
                .and_then(|value| value.checked_add(self.limits.min_free_space)),
            None => self
                .limits
                .max_file_size
                .checked_add(self.limits.min_free_space),
        };
        let Some(needed) = needed else {
            return Err(FilesError::NoSpace {
                needed: u64::MAX,
                available,
            });
        };
        if available < needed {
            return Err(FilesError::NoSpace { needed, available });
        }
        Ok(())
    }
}

#[async_trait]
impl Files for LocalFiles {
    async fn open_incoming(&self, request: IncomingRequest) -> Result<IncomingFile, FilesError> {
        if let Some(size) = request.size {
            if size > self.limits.max_file_size {
                return Err(FilesError::TooLarge {
                    size,
                    limit: self.limits.max_file_size,
                });
            }
        }
        self.reserve_space(request.size)?;

        let name = sanitize_filename(request.filename.as_deref().unwrap_or_default(), self.limits);
        let (temp_path, file) = self.create_temp().await?;
        Ok(IncomingFile {
            root: self.root.clone(),
            temp_path,
            name,
            limits: self.limits,
            file: Some(file),
            #[cfg(test)]
            strategy: self.strategy,
        })
    }

    async fn open_outgoing(&self, path: &Path) -> Result<OutgoingFile, FilesError> {
        let metadata = std::fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(FilesError::NotRegularFile(path.to_path_buf()));
        }
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| FilesError::Unreadable(path.to_path_buf()))?;
        let metadata = file.metadata().await?;
        if !metadata.is_file() {
            return Err(FilesError::NotRegularFile(path.to_path_buf()));
        }
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| FALLBACK_NAME.to_string());
        Ok(OutgoingFile {
            filename,
            size: metadata.len(),
            created_ms: system_time_ms(metadata.created()),
            modified_ms: system_time_ms(metadata.modified()),
            reader: file,
        })
    }

    async fn sweep_stale(&self, max_age: Duration) -> Result<SweepReport, FilesError> {
        let mut entries = tokio::fs::read_dir(&self.temp_dir).await?;
        let now = SystemTime::now();
        let mut removed = 0;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            // Only `.part` files are Handfast temp files; the private
            // directory holds nothing else.
            if path.extension().and_then(|extension| extension.to_str()) != Some("part") {
                continue;
            }
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if now.duration_since(modified).unwrap_or_default() <= max_age {
                continue;
            }
            match tokio::fs::remove_file(&path).await {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(FilesError::Io(error)),
            }
        }
        Ok(SweepReport { removed })
    }
}

/// Sanitize an untrusted peer filename into one safe path component.
///
/// Traversal and absolute paths become a basename. Control characters, bidi
/// controls and the Windows-invalid characters `: * ? " < > |` become `_`.
/// Trailing dots and spaces are removed. `.` and `..` fall back to
/// `shared-file`. Windows reserved names and `.handfast-tmp` get a leading `_`.
/// The result is capped at `max_name_bytes - suffix_reserve` bytes, keeping the
/// extension when possible, and is trimmed again after truncation.
#[must_use]
pub fn sanitize_filename(raw: &str, limits: FileLimits) -> String {
    let basename = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let mut name: String = basename.chars().map(sanitize_char).collect();
    name = trim_trailing(&name).to_string();
    if name.is_empty() {
        return fallback_name(basename, limits);
    }
    if is_reserved_name(&name) {
        name.insert(0, '_');
    }
    if name.eq_ignore_ascii_case(TEMP_DIR_NAME) {
        name.insert(0, '_');
    }
    let budget = limits.max_name_bytes.saturating_sub(limits.suffix_reserve);
    let name = truncate_name(&name, budget);
    if name.is_empty() {
        fallback_name(basename, limits)
    } else {
        name
    }
}

/// Unix default incoming directory from explicit environment values.
///
/// `$XDG_DOWNLOAD_DIR/Handfast` wins when the value is absolute, then
/// `$HOME/Downloads/Handfast`.
#[must_use]
pub fn unix_incoming_dir(xdg_download_dir: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(downloads) = xdg_download_dir.filter(|path| path.is_absolute()) {
        return Some(downloads.join("Handfast"));
    }
    home.map(|home| home.join("Downloads").join("Handfast"))
}

/// Windows default incoming directory from explicit environment values.
///
/// `%USERPROFILE%\Downloads\Handfast` wins, then
/// `%LOCALAPPDATA%\handfast\incoming`.
#[must_use]
pub fn windows_incoming_dir(
    user_profile: Option<&Path>,
    local_app_data: Option<&Path>,
) -> Option<PathBuf> {
    user_profile
        .map(|profile| profile.join("Downloads").join("Handfast"))
        .or_else(|| local_app_data.map(|local| local.join("handfast").join("incoming")))
}

/// Resolve the default incoming directory for this host.
///
/// Falls back to `std::env::temp_dir()/Handfast` when no platform variable is
/// set.
#[must_use]
pub fn default_incoming_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let profile = environment_path("USERPROFILE");
        let local = environment_path("LOCALAPPDATA");
        if let Some(path) = windows_incoming_dir(profile.as_deref(), local.as_deref()) {
            return path;
        }
    }
    #[cfg(not(windows))]
    {
        let xdg = environment_path("XDG_DOWNLOAD_DIR");
        let home = environment_path("HOME");
        if let Some(path) = unix_incoming_dir(xdg.as_deref(), home.as_deref()) {
            return path;
        }
    }
    std::env::temp_dir().join("Handfast")
}

fn environment_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn ensure_root(root: &Path) -> Result<(), FilesError> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(FilesError::RootSymlink(root.to_path_buf()));
            }
            if !metadata.is_dir() {
                return Err(FilesError::RootNotDirectory(root.to_path_buf()));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_dir(root)?,
        Err(error) => return Err(FilesError::Io(error)),
    }
    Ok(())
}

fn ensure_temp_dir(temp_dir: &Path) -> Result<(), FilesError> {
    match std::fs::symlink_metadata(temp_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(FilesError::TempSymlink(temp_dir.to_path_buf()));
            }
            if !metadata.is_dir() {
                return Err(FilesError::TempNotDirectory(temp_dir.to_path_buf()));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o077 != 0 {
                    return Err(FilesError::TempPermissions(temp_dir.to_path_buf()));
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_dir(temp_dir)?,
        Err(error) => return Err(FilesError::Io(error)),
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), FilesError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)?;
    }
    Ok(())
}

fn sanitize_char(character: char) -> char {
    if character.is_control()
        || is_bidi_control(character)
        || matches!(character, ':' | '*' | '?' | '"' | '<' | '>' | '|')
    {
        '_'
    } else {
        character
    }
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

fn is_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let bytes = upper.as_bytes();
    bytes.len() == 4
        && (bytes.starts_with(b"COM") || bytes.starts_with(b"LPT"))
        && matches!(bytes[3], b'1'..=b'9')
}

fn trim_trailing(name: &str) -> &str {
    name.trim_end_matches(['.', ' '])
}

fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 && index + 1 < name.len() => (&name[..index], &name[index..]),
        _ => (name, ""),
    }
}

fn truncate_name(name: &str, max_bytes: usize) -> String {
    let truncated = if name.len() <= max_bytes {
        name.to_string()
    } else {
        let (stem, extension) = split_extension(name);
        if !extension.is_empty() && extension.len() < max_bytes {
            let stem = truncate_at_boundary(stem, max_bytes - extension.len());
            format!("{stem}{extension}")
        } else {
            truncate_at_boundary(name, max_bytes).to_string()
        }
    };
    trim_trailing(&truncated).to_string()
}

fn truncate_at_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn fallback_name(original: &str, limits: FileLimits) -> String {
    let (_, extension) = split_extension(original);
    let extension: String = extension.chars().map(sanitize_char).collect();
    let candidate = format!("{FALLBACK_NAME}{extension}");
    let budget = limits.max_name_bytes.saturating_sub(limits.suffix_reserve);
    let name = truncate_name(&candidate, budget);
    if name.is_empty() {
        FALLBACK_NAME.to_string()
    } else {
        name
    }
}

fn candidate_name(base: &str, attempt: usize, budget: usize) -> String {
    let base = truncate_name(base, budget);
    let base = if base.is_empty() {
        FALLBACK_NAME.to_string()
    } else {
        base
    };
    if attempt == 0 {
        return base;
    }
    let (stem, extension) = split_extension(&base);
    format!("{stem} ({attempt}){extension}")
}

enum CommitError {
    Exists,
    NameTooLong,
    Io(io::Error),
}

async fn try_commit(temp: &Path, destination: &Path, force_copy: bool) -> Result<(), CommitError> {
    if !force_copy {
        match std::fs::hard_link(temp, destination) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CommitError::Exists);
            }
            Err(error) if is_name_too_long(&error) => return Err(CommitError::NameTooLong),
            Err(error) if hard_link_unsupported(&error) => {}
            Err(error) => return Err(CommitError::Io(error)),
        }
    }
    // shortcut: a crash during the copy can leave a partial final file, and
    // the sweep cannot identify it as Handfast's. Upgrade by refusing to
    // receive on filesystems without hard links.
    copy_commit(temp, destination).await
}

fn hard_link_unsupported(error: &io::Error) -> bool {
    // Rust maps ENOSYS to Unsupported, so this also covers a missing syscall.
    matches!(
        error.kind(),
        io::ErrorKind::Unsupported
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::CrossesDevices
    ) || is_enosys(error)
        || is_windows_unsupported(error)
}

#[cfg(target_os = "linux")]
fn is_enosys(error: &io::Error) -> bool {
    error.raw_os_error() == Some(38)
}

#[cfg(not(target_os = "linux"))]
fn is_enosys(_error: &io::Error) -> bool {
    false
}

#[cfg(windows)]
fn is_windows_unsupported(error: &io::Error) -> bool {
    // CreateHardLinkW returns ERROR_INVALID_FUNCTION (1) or ERROR_NOT_SUPPORTED
    // (50) on FAT and exFAT; Rust maps both to ErrorKind::Uncategorized.
    matches!(error.raw_os_error(), Some(1) | Some(50))
}

#[cfg(not(windows))]
fn is_windows_unsupported(_error: &io::Error) -> bool {
    false
}

fn is_name_too_long(error: &io::Error) -> bool {
    // ENAMETOOLONG and ERROR_FILENAME_EXCED_RANGE both map here.
    error.kind() == io::ErrorKind::InvalidFilename
}

async fn copy_commit(temp: &Path, destination: &Path) -> Result<(), CommitError> {
    let mut source = tokio::fs::File::open(temp).await.map_err(CommitError::Io)?;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut target = options
        .open(destination)
        .await
        .map_err(|error| map_create_error(error, destination))?;
    if let Err(error) = tokio::io::copy(&mut source, &mut target).await {
        drop(target);
        let _ = tokio::fs::remove_file(destination).await;
        return Err(CommitError::Io(error));
    }
    if let Err(error) = target.sync_all().await {
        drop(target);
        let _ = tokio::fs::remove_file(destination).await;
        return Err(CommitError::Io(error));
    }
    Ok(())
}

fn map_create_error(error: io::Error, destination: &Path) -> CommitError {
    match error.kind() {
        io::ErrorKind::AlreadyExists | io::ErrorKind::IsADirectory => CommitError::Exists,
        io::ErrorKind::InvalidFilename => CommitError::NameTooLong,
        io::ErrorKind::PermissionDenied if std::fs::symlink_metadata(destination).is_ok() => {
            CommitError::Exists
        }
        _ => CommitError::Io(error),
    }
}

async fn remove_temp(path: &Path) {
    // A failed remove leaves the file for the next startup sweep.
    let _ = tokio::fs::remove_file(path).await;
}

#[cfg(unix)]
async fn sync_root(root: &Path) {
    // The file is already committed; a directory sync failure only weakens
    // durability, so it must not turn a committed file into an error.
    if let Ok(directory) = tokio::fs::File::open(root).await {
        let _ = directory.sync_all().await;
    }
}

#[cfg(not(unix))]
async fn sync_root(_root: &Path) {}

fn system_time_ms(time: io::Result<SystemTime>) -> Option<i64> {
    let time = time.ok()?;
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_millis()).ok()
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitStrategy {
    HardLink,
    Copy,
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    fn limits() -> FileLimits {
        FileLimits::default()
    }

    fn sanitize(raw: &str) -> String {
        sanitize_filename(raw, limits())
    }

    fn files(dir: &tempfile::TempDir, strategy: CommitStrategy) -> LocalFiles {
        LocalFiles::with_strategy(dir.path().join("incoming"), limits(), strategy).unwrap()
    }

    async fn commit_bytes(
        files: &LocalFiles,
        name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, FilesError> {
        let mut incoming = files
            .open_incoming(IncomingRequest {
                filename: Some(name.to_string()),
                size: Some(bytes.len() as u64),
                open: false,
            })
            .await?;
        incoming.writer().write_all(bytes).await?;
        incoming.commit().await
    }

    fn age_file(path: &Path, age: Duration) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn sanitizer_table() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("/etc/passwd"), "passwd");
        assert_eq!(sanitize(r"C:\Users\me\report.pdf"), "report.pdf");
        assert_eq!(sanitize(r"dir\file.txt"), "file.txt");
        assert_eq!(sanitize(r"..\..\secret"), "secret");
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("con.txt"), "_con.txt");
        assert_eq!(sanitize("COM1.log"), "_COM1.log");
        assert_eq!(sanitize("LPT9"), "_LPT9");
        assert_eq!(sanitize("COM0"), "COM0");
        assert_eq!(sanitize("CONSOLE"), "CONSOLE");
        assert_eq!(sanitize("name."), "name");
        assert_eq!(sanitize("name "), "name");
        assert_eq!(sanitize("file.txt."), "file.txt");
        assert_eq!(sanitize("a\u{0}b"), "a_b");
        assert_eq!(sanitize("a\u{85}b"), "a_b");
        assert_eq!(sanitize("a\u{202E}b"), "a_b");
        assert_eq!(sanitize("a:b*c?d\"e<f>g|h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize(""), "shared-file");
        assert_eq!(sanitize("."), "shared-file");
        assert_eq!(sanitize(".."), "shared-file");
        assert_eq!(sanitize("   "), "shared-file");
        assert_eq!(sanitize(".handfast-tmp"), "_.handfast-tmp");
        assert_eq!(sanitize(".HANDFAST-TMP"), "_.HANDFAST-TMP");
        assert_eq!(sanitize("naïve-ファイル.txt"), "naïve-ファイル.txt");
    }

    #[test]
    fn long_name_is_truncated_and_keeps_extension() {
        let long = format!("{}.txt", "a".repeat(296));
        let name = sanitize(&long);
        assert_eq!(name.len(), 239);
        assert!(name.ends_with(".txt"));
    }

    #[test]
    fn unicode_name_truncates_at_a_character_boundary() {
        let long = format!("{}.txt", "é".repeat(300));
        let name = sanitize(&long);
        assert!(name.len() <= 239);
        assert!(name.ends_with(".txt"));
        assert!(name.is_char_boundary(name.len()));
    }

    #[test]
    fn truncation_trims_trailing_dot_and_space() {
        let trailing_space = format!("{}{}{}", "a".repeat(238), " ", "b".repeat(300));
        let name = sanitize(&trailing_space);
        assert_eq!(name.len(), 238);
        assert!(!name.ends_with(' '));

        let trailing_dot = format!("{}.{}", "a".repeat(238), "b".repeat(300));
        let name = sanitize(&trailing_dot);
        assert_eq!(name.len(), 238);
        assert!(!name.ends_with('.'));
    }

    #[test]
    fn candidate_names_shorten_with_the_budget() {
        let base = "a".repeat(239);
        let first = candidate_name(&base, 0, 239);
        assert_eq!(first.len(), 239);

        let second = candidate_name(&base, 1, 100);
        assert_eq!(second.len(), 104);
        assert!(second.ends_with(" (1)"));
    }

    #[test]
    fn hard_link_unsupported_classifier() {
        assert!(hard_link_unsupported(&io::Error::from(
            io::ErrorKind::Unsupported
        )));
        assert!(hard_link_unsupported(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(hard_link_unsupported(&io::Error::from(
            io::ErrorKind::CrossesDevices
        )));
        assert!(!hard_link_unsupported(&io::Error::from(
            io::ErrorKind::NotFound
        )));
    }

    #[cfg(windows)]
    #[test]
    fn windows_hard_link_unsupported_errors_fall_back_to_copy() {
        assert!(hard_link_unsupported(&io::Error::from_raw_os_error(1)));
        assert!(hard_link_unsupported(&io::Error::from_raw_os_error(50)));
    }

    #[tokio::test]
    async fn open_incoming_writes_into_the_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let files = LocalFiles::new(dir.path().join("incoming"), limits()).unwrap();
        let mut incoming = files
            .open_incoming(IncomingRequest {
                filename: Some("photo.jpg".to_string()),
                size: Some(5),
                open: false,
            })
            .await
            .unwrap();

        assert!(incoming.temp_path.starts_with(files.temp_dir()));
        assert!(incoming
            .temp_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".part"));
        incoming.writer().write_all(b"hello").await.unwrap();
        assert!(!files.root().join("photo.jpg").exists());
        assert_eq!(std::fs::read_dir(files.temp_dir()).unwrap().count(), 1);

        let path = incoming.commit().await.unwrap();
        assert_eq!(path, files.root().join("photo.jpg"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert_eq!(std::fs::read_dir(files.temp_dir()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_permissions_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("incoming");
        let files = LocalFiles::new(&root, limits()).unwrap();
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(files.temp_dir()), 0o700);

        let incoming = files
            .open_incoming(IncomingRequest {
                filename: Some("photo.jpg".to_string()),
                size: Some(1),
                open: false,
            })
            .await
            .unwrap();
        assert_eq!(mode(&incoming.temp_path), 0o600);
        incoming.abort().await;
    }

    #[cfg(unix)]
    #[test]
    fn existing_root_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("incoming");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _files = LocalFiles::new(&root, limits()).unwrap();
        assert_eq!(mode(&root), 0o755);
    }

    #[tokio::test]
    async fn duplicate_names_get_suffixes() {
        for strategy in [CommitStrategy::HardLink, CommitStrategy::Copy] {
            let dir = tempfile::tempdir().unwrap();
            let files = files(&dir, strategy);
            let first = commit_bytes(&files, "report.txt", b"one").await.unwrap();
            let second = commit_bytes(&files, "report.txt", b"two").await.unwrap();
            let third = commit_bytes(&files, "report.txt", b"three").await.unwrap();

            assert_eq!(first.file_name().unwrap().to_string_lossy(), "report.txt");
            assert_eq!(
                second.file_name().unwrap().to_string_lossy(),
                "report (1).txt"
            );
            assert_eq!(
                third.file_name().unwrap().to_string_lossy(),
                "report (2).txt"
            );
            assert_eq!(std::fs::read(&first).unwrap(), b"one");
            assert_eq!(std::fs::read(&second).unwrap(), b"two");
            assert_eq!(std::fs::read(&third).unwrap(), b"three");
        }
    }

    #[tokio::test]
    async fn concurrent_duplicates_do_not_overwrite() {
        for strategy in [CommitStrategy::HardLink, CommitStrategy::Copy] {
            let dir = tempfile::tempdir().unwrap();
            let files = files(&dir, strategy);
            let (left, right) = tokio::join!(
                commit_bytes(&files, "same.txt", b"left"),
                commit_bytes(&files, "same.txt", b"right"),
            );
            let left = left.unwrap();
            let right = right.unwrap();
            assert_ne!(left, right);

            let mut names = vec![
                left.file_name().unwrap().to_string_lossy().into_owned(),
                right.file_name().unwrap().to_string_lossy().into_owned(),
            ];
            names.sort();
            assert_eq!(names, vec!["same (1).txt", "same.txt"]);

            let mut bodies = vec![
                std::fs::read(&left).unwrap(),
                std::fs::read(&right).unwrap(),
            ];
            bodies.sort();
            assert_eq!(bodies, vec![b"left".to_vec(), b"right".to_vec()]);
        }
    }

    #[tokio::test]
    async fn existing_directory_gets_a_suffix() {
        for strategy in [CommitStrategy::HardLink, CommitStrategy::Copy] {
            let dir = tempfile::tempdir().unwrap();
            let files = files(&dir, strategy);
            std::fs::create_dir(files.root().join("report.txt")).unwrap();
            let path = commit_bytes(&files, "report.txt", b"data").await.unwrap();
            assert_eq!(
                path.file_name().unwrap().to_string_lossy(),
                "report (1).txt"
            );
            assert!(files.root().join("report.txt").is_dir());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_symlink_gets_a_suffix() {
        for strategy in [CommitStrategy::HardLink, CommitStrategy::Copy] {
            let dir = tempfile::tempdir().unwrap();
            let files = files(&dir, strategy);
            let target = dir.path().join("target.txt");
            std::fs::write(&target, b"target").unwrap();
            let link = files.root().join("report.txt");
            std::os::unix::fs::symlink(&target, &link).unwrap();

            let path = commit_bytes(&files, "report.txt", b"data").await.unwrap();
            assert_eq!(
                path.file_name().unwrap().to_string_lossy(),
                "report (1).txt"
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"target");
            assert!(std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }

    #[tokio::test]
    async fn max_length_name_with_duplicate_fits() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(&dir, CommitStrategy::HardLink);
        let name = format!("{}.txt", "a".repeat(296));
        let first = commit_bytes(&files, &name, b"1").await.unwrap();
        let second = commit_bytes(&files, &name, b"2").await.unwrap();

        assert_eq!(first.file_name().unwrap().to_string_lossy().len(), 239);
        let second = second.file_name().unwrap().to_string_lossy().into_owned();
        assert!(second.ends_with(" (1).txt"));
        assert!(second.len() <= 255);
    }

    #[tokio::test]
    async fn abort_removes_the_temp() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(&dir, CommitStrategy::HardLink);
        let mut incoming = files
            .open_incoming(IncomingRequest {
                filename: Some("partial.bin".to_string()),
                size: Some(7),
                open: false,
            })
            .await
            .unwrap();
        let temp_path = incoming.temp_path.clone();
        incoming.writer().write_all(b"partial").await.unwrap();
        incoming.abort().await;

        assert!(!temp_path.exists());
        assert!(!files.root().join("partial.bin").exists());
    }

    #[tokio::test]
    async fn drop_removes_the_temp() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(&dir, CommitStrategy::HardLink);
        let mut incoming = files
            .open_incoming(IncomingRequest {
                filename: Some("partial.bin".to_string()),
                size: Some(7),
                open: false,
            })
            .await
            .unwrap();
        let temp_path = incoming.temp_path.clone();
        incoming.writer().write_all(b"partial").await.unwrap();
        drop(incoming);

        assert!(!temp_path.exists());
        assert!(!files.root().join("partial.bin").exists());
    }

    #[tokio::test]
    async fn free_space_check_reserves_the_extra_allowance() {
        let dir = tempfile::tempdir().unwrap();
        let limits = FileLimits {
            max_extra: u64::MAX / 2,
            min_free_space: 0,
            ..limits()
        };
        let files = LocalFiles::new(dir.path().join("incoming"), limits).unwrap();
        let error = files
            .open_incoming(IncomingRequest {
                filename: Some("tiny.bin".to_string()),
                size: Some(1),
                open: false,
            })
            .await
            .err()
            .unwrap();
        assert!(matches!(error, FilesError::NoSpace { .. }));
        assert_eq!(std::fs::read_dir(files.temp_dir()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn free_space_check_reserves_the_minimum() {
        let dir = tempfile::tempdir().unwrap();
        let limits = FileLimits {
            max_extra: 0,
            min_free_space: u64::MAX / 2,
            ..limits()
        };
        let files = LocalFiles::new(dir.path().join("incoming"), limits).unwrap();
        let error = files
            .open_incoming(IncomingRequest {
                filename: Some("tiny.bin".to_string()),
                size: Some(1),
                open: false,
            })
            .await
            .err()
            .unwrap();
        assert!(matches!(error, FilesError::NoSpace { .. }));
        assert_eq!(std::fs::read_dir(files.temp_dir()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn unknown_size_reserves_the_file_cap() {
        let dir = tempfile::tempdir().unwrap();
        let limits = FileLimits {
            max_file_size: u64::MAX / 2,
            ..limits()
        };
        let files = LocalFiles::new(dir.path().join("incoming"), limits).unwrap();
        let error = files
            .open_incoming(IncomingRequest {
                filename: Some("stream.bin".to_string()),
                size: None,
                open: false,
            })
            .await
            .err()
            .unwrap();
        assert!(matches!(error, FilesError::NoSpace { .. }));
    }

    #[tokio::test]
    async fn max_file_size_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let limits = FileLimits {
            max_file_size: 4,
            ..limits()
        };
        let files = LocalFiles::new(dir.path().join("incoming"), limits).unwrap();
        let error = files
            .open_incoming(IncomingRequest {
                filename: Some("large.bin".to_string()),
                size: Some(5),
                open: false,
            })
            .await
            .err()
            .unwrap();
        assert!(matches!(error, FilesError::TooLarge { size: 5, limit: 4 }));
        assert_eq!(std::fs::read_dir(files.temp_dir()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn sweep_removes_only_old_part_files() {
        let dir = tempfile::tempdir().unwrap();
        let files = LocalFiles::new(dir.path().join("incoming"), limits()).unwrap();

        let old_temp = files.temp_dir().join("old.part");
        std::fs::write(&old_temp, b"old").unwrap();
        age_file(&old_temp, Duration::from_secs(2 * 3600));

        let young_temp = files.temp_dir().join("young.part");
        std::fs::write(&young_temp, b"young").unwrap();

        let old_other = files.temp_dir().join("keep.txt");
        std::fs::write(&old_other, b"keep").unwrap();
        age_file(&old_other, Duration::from_secs(2 * 3600));

        let user_file = files.root().join("user.part");
        std::fs::write(&user_file, b"user").unwrap();
        age_file(&user_file, Duration::from_secs(2 * 3600));

        let report = files.sweep_stale(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(report.removed, 1);
        assert!(!old_temp.exists());
        assert!(young_temp.exists());
        assert!(old_other.exists());
        assert!(user_file.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sweep_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let files = LocalFiles::new(dir.path().join("incoming"), limits()).unwrap();
        let target = files.root().join("user.txt");
        std::fs::write(&target, b"user").unwrap();
        age_file(&target, Duration::from_secs(2 * 3600));
        let link = files.temp_dir().join("link.part");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let report = files.sweep_stale(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(report.removed, 0);
        assert!(target.exists());
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn temp_path_file_fails_startup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("incoming");
        std::fs::create_dir(&root).unwrap();
        let temp = root.join(TEMP_DIR_NAME);
        std::fs::write(&temp, b"user").unwrap();

        let error = LocalFiles::new(&root, limits()).unwrap_err();
        assert!(matches!(error, FilesError::TempNotDirectory(_)));
        assert_eq!(std::fs::read(&temp).unwrap(), b"user");
    }

    #[cfg(unix)]
    #[test]
    fn temp_path_symlink_fails_startup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("incoming");
        std::fs::create_dir(&root).unwrap();
        let temp = root.join(TEMP_DIR_NAME);
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), &temp).unwrap();

        let error = LocalFiles::new(&root, limits()).unwrap_err();
        assert!(matches!(error, FilesError::TempSymlink(_)));
        assert!(std::fs::symlink_metadata(&temp)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_fails_startup() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let error = LocalFiles::new(&link, limits()).unwrap_err();
        assert!(matches!(error, FilesError::RootSymlink(_)));
    }

    #[cfg(unix)]
    #[test]
    fn existing_temp_dir_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("incoming");
        std::fs::create_dir(&root).unwrap();
        let temp = root.join(TEMP_DIR_NAME);
        std::fs::create_dir(&temp).unwrap();
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755)).unwrap();

        let error = LocalFiles::new(&root, limits()).unwrap_err();
        assert!(matches!(error, FilesError::TempPermissions(_)));
    }

    #[tokio::test]
    async fn outgoing_zero_byte_file_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, b"").unwrap();
        let files = LocalFiles::new(dir.path().join("incoming"), limits()).unwrap();

        let outgoing = files.open_outgoing(&path).await.unwrap();
        assert_eq!(outgoing.filename, "empty.txt");
        assert_eq!(outgoing.size, 0);
    }

    #[tokio::test]
    async fn outgoing_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("directory");
        std::fs::create_dir(&path).unwrap();
        let files = LocalFiles::new(dir.path().join("incoming"), limits()).unwrap();

        let error = files.open_outgoing(&path).await.err().unwrap();
        assert!(matches!(error, FilesError::NotRegularFile(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn outgoing_special_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let files = LocalFiles::new(dir.path().join("incoming"), limits()).unwrap();

        let error = files.open_outgoing(&path).await.err().unwrap();
        assert!(matches!(error, FilesError::NotRegularFile(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn outgoing_unreadable_file_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, b"secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let files = LocalFiles::new(dir.path().join("incoming"), limits()).unwrap();

        let error = files.open_outgoing(&path).await.err().unwrap();
        assert!(matches!(error, FilesError::Unreadable(_)));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn unix_default_prefers_xdg_then_home() {
        let xdg = std::env::temp_dir().join("xdg-downloads");
        let home = std::env::temp_dir().join("home");
        assert_eq!(
            unix_incoming_dir(Some(&xdg), Some(&home)),
            Some(xdg.join("Handfast"))
        );
        assert_eq!(
            unix_incoming_dir(None, Some(&home)),
            Some(home.join("Downloads").join("Handfast"))
        );
        assert_eq!(unix_incoming_dir(None, None), None);
        assert_eq!(
            unix_incoming_dir(Some(Path::new("relative")), Some(&home)),
            Some(home.join("Downloads").join("Handfast"))
        );
    }

    #[test]
    fn windows_default_prefers_profile_then_local_app_data() {
        let profile = std::env::temp_dir().join("profile");
        let local = std::env::temp_dir().join("local");
        assert_eq!(
            windows_incoming_dir(Some(&profile), Some(&local)),
            Some(profile.join("Downloads").join("Handfast"))
        );
        assert_eq!(
            windows_incoming_dir(None, Some(&local)),
            Some(local.join("handfast").join("incoming"))
        );
        assert_eq!(windows_incoming_dir(None, None), None);
    }
}
