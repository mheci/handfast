//! Verified self-update.
//!
//! [`Updater::check`] reads the latest GitHub release and compares its `vX.Y.Z`
//! tag with this build's version. [`Updater::apply`] downloads the bare binary
//! for this target, checks it against `SHA256SUMS` and the Authenticode
//! signature, then replaces the running executable with a rename pair so a
//! crash leaves a state the next start finishes.
//!
//! The network and the signature check sit behind [`ReleaseSource`] and
//! [`SignatureVerifier`], so the tests never touch a real release.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use ureq::ResponseExt;

use crate::control::ControlError;
use crate::service::{DaemonControl, Service, STOP_TIMEOUT};

/// The repository the updater reads releases from.
pub const REPO: &str = "mheci/handfast";

/// The checksum file published beside the portable assets.
pub const SUMS_ASSET: &str = "SHA256SUMS";

/// Largest accepted asset download.
pub const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;

/// Deadline for one release request or asset download.
pub const UPDATE_TIMEOUT: Duration = Duration::from_secs(60);

/// Hosts an update may be fetched from. A final URL after redirects is checked
/// against the same list.
pub const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "api.github.com",
    "objects.githubusercontent.com",
];

const RENAME_ATTEMPTS: u32 = 3;
const RENAME_DELAY: Duration = Duration::from_millis(200);
const STOP_POLL: Duration = Duration::from_millis(100);

/// A release version, compared by its numeric triple only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    /// First component.
    pub major: u64,
    /// Second component.
    pub minor: u64,
    /// Third component.
    pub patch: u64,
}

impl Version {
    /// Parse a `vX.Y.Z` tag. A `-` or `+` suffix is dropped, so
    /// `v0.3.0-alpha` parses as `0.3.0`.
    ///
    /// # Errors
    ///
    /// Fails when the tag does not carry three numeric components.
    pub fn parse_tag(tag: &str) -> Result<Self, UpdateError> {
        let text = tag.strip_prefix('v').unwrap_or(tag);
        let numeric = text.split(['-', '+']).next().unwrap_or_default();
        let mut parts = numeric.split('.');
        let major = parse_component(parts.next(), tag)?;
        let minor = parse_component(parts.next(), tag)?;
        let patch = parse_component(parts.next(), tag)?;
        if parts.next().is_some() {
            return Err(UpdateError::Version(tag.to_string()));
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

fn parse_component(part: Option<&str>, tag: &str) -> Result<u64, UpdateError> {
    part.and_then(|part| part.parse().ok())
        .ok_or_else(|| UpdateError::Version(tag.to_string()))
}

/// This build's version, from the package metadata.
///
/// # Errors
///
/// Fails when `CARGO_PKG_VERSION` is not `X.Y.Z`, which no build produces.
pub fn current_version() -> Result<Version, UpdateError> {
    Version::parse_tag(env!("CARGO_PKG_VERSION"))
}

/// The release asset name for a target triple, with the platform's exe suffix.
#[must_use]
pub fn asset_name(target: &str) -> String {
    format!("handfast-{target}{}", std::env::consts::EXE_SUFFIX)
}

/// One downloadable file in a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// File name, as the release publishes it.
    pub name: String,
    /// HTTPS download URL.
    pub url: String,
}

/// The latest published release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The `vX.Y.Z` tag.
    pub tag: String,
    /// Published assets.
    pub assets: Vec<Asset>,
}

impl Release {
    /// Find one asset by its exact file name.
    #[must_use]
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    /// Parse the tag.
    ///
    /// # Errors
    ///
    /// Fails when the tag is not `vX.Y.Z`.
    pub fn version(&self) -> Result<Version, UpdateError> {
        Version::parse_tag(&self.tag)
    }
}

/// Where releases and their bytes come from.
pub trait ReleaseSource {
    /// Read the latest release.
    ///
    /// # Errors
    ///
    /// Fails on a transport error or a body that is not a release.
    fn latest_release(&self) -> Result<Release, UpdateError>;

    /// Stream one asset into `sink` and return the byte count.
    ///
    /// # Errors
    ///
    /// Fails on a transport error, a refused URL or a body over
    /// [`MAX_ASSET_BYTES`].
    fn download(&self, url: &str, sink: &mut dyn Write) -> Result<u64, UpdateError>;
}

/// The real release source, over HTTPS to GitHub.
pub struct GitHub {
    agent: ureq::Agent,
}

impl GitHub {
    /// Build a client with the update deadline.
    #[must_use]
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(UPDATE_TIMEOUT))
            .user_agent(concat!("handfast/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for GitHub {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: String,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Deserialize)]
struct AssetJson {
    name: String,
    browser_download_url: String,
}

impl ReleaseSource for GitHub {
    fn latest_release(&self) -> Result<Release, UpdateError> {
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        check_url(&url)?;
        let mut response = self
            .agent
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(http_error)?;
        check_url(&response.get_uri().to_string())?;
        let body = response.body_mut().read_to_string().map_err(http_error)?;
        let release: ReleaseJson = serde_json::from_str(&body)?;
        Ok(Release {
            tag: release.tag_name,
            assets: release
                .assets
                .into_iter()
                .map(|asset| Asset {
                    name: asset.name,
                    url: asset.browser_download_url,
                })
                .collect(),
        })
    }

    fn download(&self, url: &str, sink: &mut dyn Write) -> Result<u64, UpdateError> {
        check_url(url)?;
        let response = self.agent.get(url).call().map_err(http_error)?;
        check_url(&response.get_uri().to_string())?;
        let mut body = response.into_body();
        let mut reader = body.as_reader().take(MAX_ASSET_BYTES + 1);
        let written = io::copy(&mut reader, &mut *sink)?;
        if written > MAX_ASSET_BYTES {
            return Err(UpdateError::TooLarge(MAX_ASSET_BYTES));
        }
        Ok(written)
    }
}

fn http_error(error: ureq::Error) -> UpdateError {
    UpdateError::Http(error.to_string())
}

/// Checks the downloaded file's platform signature.
pub trait SignatureVerifier {
    /// Verify `path`, or say why it cannot be trusted.
    ///
    /// # Errors
    ///
    /// Fails when the file is unsigned or the signature is invalid.
    fn verify(&self, path: &Path) -> Result<(), UpdateError>;
}

/// The real verifier: Authenticode on Windows, nothing on Unix.
pub struct Authenticode;

impl SignatureVerifier for Authenticode {
    fn verify(&self, path: &Path) -> Result<(), UpdateError> {
        #[cfg(windows)]
        return verify_authenticode(path);
        #[cfg(not(windows))]
        {
            let _ = path;
            Ok(())
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn verify_authenticode(path: &Path) -> Result<(), UpdateError> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    use windows_sys::Win32::Security::WinTrust::{
        WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0,
        WINTRUST_FILE_INFO, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
        WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wide.as_ptr(),
        hFile: null_mut(),
        pgKnownSubject: null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        pPolicyCallbackData: null_mut(),
        pSIPClientData: null_mut(),
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        hWVTStateData: null_mut(),
        pwszURLReference: null_mut(),
        dwProvFlags: 0,
        dwUIContext: 0,
        pSignatureSettings: null_mut(),
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe {
        WinVerifyTrust(
            null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
    }
    if status == 0 {
        return Ok(());
    }
    Err(UpdateError::Signature(format!(
        "WinVerifyTrust returned 0x{status:08x} for {}",
        path.display()
    )))
}

/// The parsed `SHA256SUMS` table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checksums(HashMap<String, [u8; 32]>);

impl Checksums {
    /// Parse a `sha256sum` listing.
    ///
    /// # Errors
    ///
    /// Fails on a line without a 64-character hex digest and a file name, or
    /// when one name appears twice.
    pub fn parse(text: &str) -> Result<Self, UpdateError> {
        let mut sums = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((hex, rest)) = line.split_once(char::is_whitespace) else {
                return Err(UpdateError::Sums(line.to_string()));
            };
            let digest =
                parse_hex(hex).ok_or_else(|| UpdateError::Sums(format!("{hex} is not sha256")))?;
            let name = rest.trim();
            let name = name.strip_prefix('*').unwrap_or(name);
            if name.is_empty() {
                return Err(UpdateError::Sums(line.to_string()));
            }
            if sums.insert(name.to_string(), digest).is_some() {
                return Err(UpdateError::Duplicate(name.to_string()));
            }
        }
        Ok(Self(sums))
    }

    /// Read one file's expected digest.
    ///
    /// # Errors
    ///
    /// Fails when the name has no line.
    pub fn get(&self, name: &str) -> Result<[u8; 32], UpdateError> {
        self.0
            .get(name)
            .copied()
            .ok_or_else(|| UpdateError::MissingSum(name.to_string()))
    }
}

fn parse_hex(value: &str) -> Option<[u8; 32]> {
    let text = value.as_bytes();
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, pair) in text.as_chunks::<2>().0.iter().enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        bytes[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(bytes)
}

/// Refuse a URL that is not HTTPS on an allowed host.
///
/// # Errors
///
/// Fails when the scheme is not `https`, the authority carries user
/// information, or the host is not [`ALLOWED_HOSTS`] or a
/// `*.githubusercontent.com` name.
pub fn check_url(url: &str) -> Result<(), UpdateError> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(UpdateError::Url(format!("{url} is not https")));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return Err(UpdateError::Url(format!("{url} has no plain host")));
    }
    let host = authority
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ALLOWED_HOSTS.contains(&host.as_str()) || host.ends_with(".githubusercontent.com") {
        return Ok(());
    }
    Err(UpdateError::Host(host))
}

/// The sibling path the current executable is renamed to during an update.
#[must_use]
pub fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_os_string();
    name.push(".old");
    PathBuf::from(name)
}

/// The download path for this process, in the executable's directory.
#[must_use]
pub fn temp_path(exe: &Path) -> PathBuf {
    let directory = exe.parent().unwrap_or_else(|| Path::new("."));
    directory.join(format!(".handfast-update-{}", std::process::id()))
}

/// Remove a leftover `handfast.old`. Every start calls this, so a crash
/// between the two renames is finished by the next run.
///
/// # Errors
///
/// Fails only when the file exists and cannot be removed. A missing file is
/// success.
pub fn cleanup_old(exe: &Path) -> io::Result<()> {
    match std::fs::remove_file(old_path(exe)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// What `update check` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// The running version.
    pub current: Version,
    /// The latest published version.
    pub latest: Version,
    /// The asset this target would download.
    pub asset: String,
}

impl Check {
    /// Whether the latest release is newer than this build.
    #[must_use]
    pub fn update_available(&self) -> bool {
        self.latest > self.current
    }
}

impl std::fmt::Display for Check {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.update_available() {
            write!(
                formatter,
                "update available: {} -> {} ({})",
                self.current, self.latest, self.asset
            )
        } else {
            write!(formatter, "up to date ({})", self.current)
        }
    }
}

/// What `update apply` installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The installed version.
    pub version: Version,
    /// The asset that was installed.
    pub asset: String,
    /// Whether the daemon was running and was started again.
    pub restarted: bool,
}

impl std::fmt::Display for Applied {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "updated to {} ({})", self.version, self.asset)?;
        if self.restarted {
            write!(formatter, ", daemon restarted")?;
        }
        Ok(())
    }
}

/// One update run over a release source, a signature check and the daemon.
pub struct Updater<'a> {
    source: &'a dyn ReleaseSource,
    verifier: &'a dyn SignatureVerifier,
    control: &'a dyn DaemonControl,
    service: &'a Service<'a>,
}

impl<'a> Updater<'a> {
    /// Describe an update run.
    #[must_use]
    pub fn new(
        source: &'a dyn ReleaseSource,
        verifier: &'a dyn SignatureVerifier,
        control: &'a dyn DaemonControl,
        service: &'a Service<'a>,
    ) -> Self {
        Self {
            source,
            verifier,
            control,
            service,
        }
    }

    /// Read the latest release and compare it with this build.
    ///
    /// # Errors
    ///
    /// Fails on a transport error or a tag that is not `vX.Y.Z`.
    pub fn check(&self) -> Result<Check, UpdateError> {
        let release = self.source.latest_release()?;
        check_release(&release)
    }

    /// Download, verify and install the latest release over `exe`.
    ///
    /// # Errors
    ///
    /// Fails and keeps the current executable when the release is not newer,
    /// the target asset or its checksum line is missing, the checksum or the
    /// signature does not match, the exe directory is not writable, the daemon
    /// does not stop, or the rename fails.
    pub fn apply(&self, exe: &Path) -> Result<Applied, UpdateError> {
        let release = self.source.latest_release()?;
        let check = check_release(&release)?;
        if !check.update_available() {
            return Err(UpdateError::NotNewer {
                current: check.current,
                latest: check.latest,
            });
        }
        let asset = release
            .asset(&check.asset)
            .ok_or_else(|| UpdateError::MissingAsset(check.asset.clone()))?;
        let sums = release
            .asset(SUMS_ASSET)
            .ok_or_else(|| UpdateError::MissingAsset(SUMS_ASSET.to_string()))?;

        let temp = temp_path(exe);
        self.prepare(asset, sums, &temp)?;
        let was_running = self.stop_daemon()?;
        replace(exe, &temp)?;
        let restarted = if was_running {
            self.service
                .start(self.control)
                .map_err(|error| UpdateError::Restart(error.to_string()))?;
            true
        } else {
            false
        };
        Ok(Applied {
            version: check.latest,
            asset: asset.name.clone(),
            restarted,
        })
    }

    /// A refusal removes the temp file and leaves the current exe alone.
    fn prepare(&self, asset: &Asset, sums: &Asset, temp: &Path) -> Result<(), UpdateError> {
        match self.download_verified(asset, sums, temp) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = std::fs::remove_file(temp);
                Err(error)
            }
        }
    }

    fn download_verified(
        &self,
        asset: &Asset,
        sums: &Asset,
        temp: &Path,
    ) -> Result<(), UpdateError> {
        let mut file = create_temp(temp)?;
        self.source.download(&asset.url, &mut file)?;
        file.sync_all()?;
        drop(file);
        let digest = sha256_file(temp)?;

        let mut listing = Vec::new();
        self.source.download(&sums.url, &mut listing)?;
        let text = String::from_utf8(listing)
            .map_err(|_| UpdateError::Sums(format!("{SUMS_ASSET} is not UTF-8")))?;
        let expected = Checksums::parse(&text)?.get(&asset.name)?;
        if digest != expected {
            return Err(UpdateError::Checksum(asset.name.clone()));
        }

        self.verifier.verify(temp)?;
        set_executable(temp)?;
        Ok(())
    }

    /// Stop the daemon and wait for its endpoint to close. Returns whether it
    /// was running.
    fn stop_daemon(&self) -> Result<bool, UpdateError> {
        match self.control.status() {
            Ok(_) => {}
            Err(ControlError::NotRunning) => return Ok(false),
            Err(error) => return Err(UpdateError::Control(error.to_string())),
        }
        match self.control.shutdown() {
            Ok(()) => {}
            Err(ControlError::NotRunning) => return Ok(false),
            Err(error) => return Err(UpdateError::Control(error.to_string())),
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            if matches!(self.control.status(), Err(ControlError::NotRunning)) {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(UpdateError::DaemonRunning(STOP_TIMEOUT));
            }
            std::thread::sleep(STOP_POLL.min(deadline - now));
        }
    }
}

fn check_release(release: &Release) -> Result<Check, UpdateError> {
    Ok(Check {
        current: current_version()?,
        latest: release.version()?,
        asset: asset_name(env!("HANDFAST_TARGET")),
    })
}

fn create_temp(temp: &Path) -> Result<std::fs::File, UpdateError> {
    std::fs::File::create(temp).map_err(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
        ) {
            UpdateError::NotWritable {
                directory: temp.parent().map(Path::to_path_buf).unwrap_or_default(),
            }
        } else {
            UpdateError::Io(error)
        }
    })
}

fn sha256_file(path: &Path) -> Result<[u8; 32], UpdateError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(hasher.finalize().as_slice());
    Ok(bytes)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), UpdateError> {
    Ok(())
}

/// Swap the downloaded file into the executable's place.
///
/// The running exe is renamed aside first, so a process that keeps its mapped
/// image keeps running. A failed second rename rolls the first one back.
fn replace(exe: &Path, temp: &Path) -> Result<(), UpdateError> {
    let old = old_path(exe);
    match std::fs::remove_file(&old) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(UpdateError::OldBusy(old)),
    }
    rename_retry(exe, &old)?;
    if let Err(error) = rename_retry(temp, exe) {
        let _ = rename_retry(&old, exe);
        return Err(error);
    }
    let _ = std::fs::remove_file(&old);
    Ok(())
}

fn rename_retry(from: &Path, to: &Path) -> Result<(), UpdateError> {
    let mut last = None;
    for attempt in 0..RENAME_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(RENAME_DELAY);
        }
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
    }
    Err(UpdateError::Rename {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        message: last.map_or_else(String::new, |error| error.to_string()),
    })
}

/// Why an update was refused.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// A filesystem or stream operation failed.
    #[error("update io: {0}")]
    Io(#[from] io::Error),

    /// The release body is not valid JSON.
    #[error("update json: {0}")]
    Json(#[from] serde_json::Error),

    /// The tag is not `vX.Y.Z`.
    #[error("update version: {0} is not vX.Y.Z")]
    Version(String),

    /// The request failed.
    #[error("update http: {0}")]
    Http(String),

    /// The host is not on the allowlist.
    #[error("{0} is not an allowed update host")]
    Host(String),

    /// The URL is not HTTPS or carries user information.
    #[error("update url: {0}")]
    Url(String),

    /// The release does not publish a required file.
    #[error("the release has no {0} asset")]
    MissingAsset(String),

    /// `SHA256SUMS` has no line for the asset.
    #[error("{SUMS_ASSET} has no line for {0}")]
    MissingSum(String),

    /// `SHA256SUMS` lists one name twice.
    #[error("{SUMS_ASSET} lists {0} twice")]
    Duplicate(String),

    /// A `SHA256SUMS` line is malformed.
    #[error("{SUMS_ASSET} is malformed: {0}")]
    Sums(String),

    /// The downloaded bytes do not match the published digest.
    #[error("the checksum for {0} does not match the download")]
    Checksum(String),

    /// The platform signature check failed.
    #[error("the signature check failed: {0}")]
    Signature(String),

    /// The executable's directory cannot take a new file.
    #[error("cannot write to {directory}: {hint}", directory = .directory.display(), hint = package_manager_hint())]
    NotWritable {
        /// The directory that refused the write.
        directory: PathBuf,
    },

    /// The current build is not older than the release.
    #[error("handfast {current} is not older than the latest release {latest}")]
    NotNewer {
        /// The running version.
        current: Version,
        /// The latest published version.
        latest: Version,
    },

    /// The download is over the cap.
    #[error("the download exceeds {0} bytes")]
    TooLarge(u64),

    /// The daemon kept answering after the shutdown request.
    #[error("the daemon did not stop within {0:?}")]
    DaemonRunning(Duration),

    /// A previous `handfast.old` cannot be removed.
    #[error(
        "cannot remove {}: close the previous Handfast process and retry",
        .0.display()
    )]
    OldBusy(PathBuf),

    /// A rename failed after every retry.
    #[error("cannot rename {} to {}: {message}", from.display(), to.display())]
    Rename {
        /// The source path.
        from: PathBuf,
        /// The target path.
        to: PathBuf,
        /// The last OS error.
        message: String,
    },

    /// The update installed but the daemon did not start again.
    #[error("the update installed but the daemon did not restart: {0}")]
    Restart(String),

    /// The control channel failed for a reason other than a stopped daemon.
    #[error("control: {0}")]
    Control(String),
}

/// The command that updates a package-managed install instead of the exe.
#[must_use]
pub fn package_manager_hint() -> &'static str {
    if cfg!(windows) {
        "install the update with the Handfast MSI"
    } else {
        "a package-managed install updates through the package manager, for example: sudo apt install --only-upgrade handfast"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::control::DaemonStatus;
    use crate::service::{CommandOutput, CommandRunner};

    struct NoCommands;

    impl CommandRunner for NoCommands {
        fn run(&self, _program: &str, _args: &[&str]) -> io::Result<CommandOutput> {
            Ok(CommandOutput::default())
        }
    }

    struct Stopped;

    impl DaemonControl for Stopped {
        fn status(&self) -> Result<DaemonStatus, ControlError> {
            Err(ControlError::NotRunning)
        }

        fn shutdown(&self) -> Result<(), ControlError> {
            Err(ControlError::NotRunning)
        }
    }

    struct FakeSource {
        release: Release,
        files: HashMap<String, Vec<u8>>,
    }

    impl ReleaseSource for FakeSource {
        fn latest_release(&self) -> Result<Release, UpdateError> {
            Ok(self.release.clone())
        }

        fn download(&self, url: &str, sink: &mut dyn Write) -> Result<u64, UpdateError> {
            let bytes = self
                .files
                .get(url)
                .ok_or_else(|| UpdateError::Http(format!("{url} is not served")))?;
            sink.write_all(bytes)?;
            Ok(bytes.len() as u64)
        }
    }

    struct Verifier(Result<(), &'static str>);

    impl SignatureVerifier for Verifier {
        fn verify(&self, _path: &Path) -> Result<(), UpdateError> {
            self.0
                .map_err(|message| UpdateError::Signature(message.to_string()))
        }
    }

    fn url_for(name: &str) -> String {
        format!("https://github.com/mheci/handfast/releases/download/v9.9.9/{name}")
    }

    fn hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            text.push(char::from(DIGITS[usize::from(*byte >> 4)]));
            text.push(char::from(DIGITS[usize::from(*byte & 0x0f)]));
        }
        text
    }

    fn sums_for(name: &str, contents: &[u8]) -> String {
        format!("{}  {name}\n", hex(Sha256::digest(contents).as_slice()))
    }

    fn fake(tag: &str, name: &str, contents: &[u8], sums: Option<String>) -> FakeSource {
        let mut assets = vec![Asset {
            name: name.to_string(),
            url: url_for(name),
        }];
        let mut files = HashMap::new();
        files.insert(url_for(name), contents.to_vec());
        if let Some(sums) = sums {
            assets.push(Asset {
                name: SUMS_ASSET.to_string(),
                url: url_for(SUMS_ASSET),
            });
            files.insert(url_for(SUMS_ASSET), sums.into_bytes());
        }
        FakeSource {
            release: Release {
                tag: tag.to_string(),
                assets,
            },
            files,
        }
    }

    fn with_updater<R>(
        source: &dyn ReleaseSource,
        verifier: &dyn SignatureVerifier,
        exe: &Path,
        run: impl FnOnce(&Updater<'_>) -> R,
    ) -> R {
        let commands = NoCommands;
        let directory = exe.parent().unwrap_or_else(|| Path::new("."));
        let service = Service::new(
            exe.to_path_buf(),
            directory.to_path_buf(),
            directory.join("handfast.log"),
            &commands,
        )
        .expect("describe the service");
        let control = Stopped;
        let updater = Updater::new(source, verifier, &control, &service);
        run(&updater)
    }

    fn test_exe() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("a temp dir");
        let exe = directory
            .path()
            .join(format!("handfast{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&exe, b"old").expect("write the current exe");
        (directory, exe)
    }

    #[test]
    fn version_compare_orders_the_numeric_triple() {
        let older = Version::parse_tag("v0.2.0").expect("a version");
        let newer = Version::parse_tag("v0.10.0").expect("a version");
        let patch = Version::parse_tag("0.2.1").expect("a version");

        assert!(newer > older, "0.10.0 is newer than 0.2.0");
        assert!(patch > older, "a patch bump is newer");
        assert_eq!(
            Version::parse_tag("v0.3.0-alpha").expect("a version"),
            Version::parse_tag("v0.3.0").expect("a version"),
            "a pre-release suffix compares by the numeric triple"
        );
        assert_eq!(
            Version::parse_tag("v0.3.0+build").expect("a version"),
            Version::parse_tag("v0.3.0").expect("a version"),
        );
        assert!(Version::parse_tag("v0.3").is_err(), "two components");
        assert!(Version::parse_tag("latest").is_err(), "no components");
        assert!(Version::parse_tag("v0.3.0.1").is_err(), "four components");
        assert_eq!(older.to_string(), "0.2.0");
    }

    #[test]
    fn asset_selection_uses_the_target_and_exe_suffix() {
        assert_eq!(
            asset_name("x86_64-unknown-linux-gnu"),
            format!(
                "handfast-x86_64-unknown-linux-gnu{}",
                std::env::consts::EXE_SUFFIX
            )
        );
        let release = Release {
            tag: "v1.0.0".to_string(),
            assets: vec![Asset {
                name: "handfast-a".to_string(),
                url: url_for("handfast-a"),
            }],
        };
        assert!(release.asset("handfast-a").is_some());
        assert!(release.asset("handfast-b").is_none());
    }

    #[test]
    fn sums_parse_finds_the_exact_name_and_refuses_missing_duplicate_and_bad_lines() {
        let digest = "00".repeat(32);
        let text = format!("{digest}  handfast-linux\n{digest} *handfast.exe\n");
        let sums = Checksums::parse(&text).expect("parse the listing");
        assert_eq!(sums.get("handfast-linux").expect("a line"), [0u8; 32]);
        assert_eq!(sums.get("handfast.exe").expect("a line"), [0u8; 32]);
        assert!(
            matches!(sums.get("handfast-other"), Err(UpdateError::MissingSum(_))),
            "a missing line refuses"
        );

        let duplicate = format!("{digest}  handfast\n{digest}  handfast\n");
        assert!(
            matches!(Checksums::parse(&duplicate), Err(UpdateError::Duplicate(_))),
            "a duplicate name refuses"
        );
        assert!(Checksums::parse("not-a-digest  handfast\n").is_err());
        assert!(Checksums::parse("handfast\n").is_err());
    }

    #[test]
    fn the_host_allowlist_accepts_github_and_refuses_everything_else() {
        for url in [
            "https://github.com/mheci/handfast/releases/download/v1/handfast-x",
            "https://api.github.com/repos/mheci/handfast/releases/latest",
            "https://objects.githubusercontent.com/github-production-asset/x",
            "https://foo.githubusercontent.com/x",
        ] {
            assert!(check_url(url).is_ok(), "{url} is allowed");
        }
        for url in [
            "http://github.com/x",
            "https://evil.com/github.com",
            "https://github.com.evil.com/x",
            "https://evilgithubusercontent.com/x",
            "https://raw.githubusercontent.com.evil.com/x",
            "https://user@github.com/x",
        ] {
            assert!(check_url(url).is_err(), "{url} is refused");
        }
    }

    #[test]
    fn check_reports_an_available_update() {
        let name = asset_name(env!("HANDFAST_TARGET"));
        let source = fake("v99.0.0", &name, b"new", None);
        let (_directory, exe) = test_exe();
        let check = with_updater(&source, &Verifier(Ok(())), &exe, |updater| {
            updater.check().expect("read the release")
        });

        assert!(check.update_available());
        assert_eq!(check.asset, name);
        assert_eq!(
            check.to_string(),
            format!(
                "update available: {} -> 99.0.0 ({name})",
                current_version().expect("a version")
            )
        );
    }

    #[test]
    fn check_reports_up_to_date_for_the_same_version() {
        let name = asset_name(env!("HANDFAST_TARGET"));
        let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
        let source = fake(&tag, &name, b"new", None);
        let (_directory, exe) = test_exe();
        let check = with_updater(&source, &Verifier(Ok(())), &exe, |updater| {
            updater.check().expect("read the release")
        });

        assert!(!check.update_available());
        assert_eq!(
            check.to_string(),
            format!("up to date ({})", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn apply_replaces_the_exe_and_removes_the_old_copy() {
        let name = asset_name(env!("HANDFAST_TARGET"));
        let source = fake("v99.0.0", &name, b"new", Some(sums_for(&name, b"new")));
        let (_directory, exe) = test_exe();
        let applied = with_updater(&source, &Verifier(Ok(())), &exe, |updater| {
            updater.apply(&exe).expect("install the update")
        });

        assert_eq!(
            applied.version,
            Version {
                major: 99,
                minor: 0,
                patch: 0
            }
        );
        assert_eq!(applied.asset, name);
        assert!(!applied.restarted, "the daemon was not running");
        assert_eq!(std::fs::read(&exe).expect("read the exe"), b"new");
        assert!(!old_path(&exe).exists(), "the old copy is removed");
        assert!(!temp_path(&exe).exists(), "the temp file is consumed");
    }

    #[test]
    fn cleanup_old_removes_a_leftover_copy_and_ignores_a_missing_one() {
        let (_directory, exe) = test_exe();
        assert!(cleanup_old(&exe).is_ok(), "a missing copy is not an error");

        std::fs::write(old_path(&exe), b"old").expect("leave a copy");
        assert!(old_path(&exe).exists());
        cleanup_old(&exe).expect("remove the leftover");
        assert!(!old_path(&exe).exists());
    }

    #[test]
    fn apply_refuses_a_checksum_mismatch_and_keeps_the_current_exe() {
        let name = asset_name(env!("HANDFAST_TARGET"));
        let source = fake(
            "v99.0.0",
            &name,
            b"new",
            Some(format!("{}  {name}\n", "11".repeat(32))),
        );
        let (_directory, exe) = test_exe();
        let error = with_updater(&source, &Verifier(Ok(())), &exe, |updater| {
            updater.apply(&exe).expect_err("refuse the download")
        });

        assert!(matches!(error, UpdateError::Checksum(_)), "{error}");
        assert_eq!(std::fs::read(&exe).expect("read the exe"), b"old");
        assert!(
            !temp_path(&exe).exists(),
            "the rejected download is removed"
        );
    }

    #[test]
    fn apply_refuses_a_failed_signature_and_keeps_the_current_exe() {
        let name = asset_name(env!("HANDFAST_TARGET"));
        let source = fake("v99.0.0", &name, b"new", Some(sums_for(&name, b"new")));
        let (_directory, exe) = test_exe();
        let error = with_updater(&source, &Verifier(Err("unsigned")), &exe, |updater| {
            updater.apply(&exe).expect_err("refuse the signature")
        });

        assert!(matches!(error, UpdateError::Signature(_)), "{error}");
        assert_eq!(std::fs::read(&exe).expect("read the exe"), b"old");
        assert!(
            !temp_path(&exe).exists(),
            "the rejected download is removed"
        );
    }

    #[test]
    fn apply_refuses_when_the_latest_is_not_newer() {
        let name = asset_name(env!("HANDFAST_TARGET"));
        let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
        let source = fake(&tag, &name, b"new", Some(sums_for(&name, b"new")));
        let (_directory, exe) = test_exe();
        let error = with_updater(&source, &Verifier(Ok(())), &exe, |updater| {
            updater.apply(&exe).expect_err("refuse an equal version")
        });

        assert!(matches!(error, UpdateError::NotNewer { .. }), "{error}");
        assert_eq!(std::fs::read(&exe).expect("read the exe"), b"old");
    }

    #[test]
    fn apply_refuses_a_release_without_the_target_asset() {
        let source = fake("v99.0.0", "handfast-other", b"new", None);
        let (_directory, exe) = test_exe();
        let error = with_updater(&source, &Verifier(Ok(())), &exe, |updater| {
            updater.apply(&exe).expect_err("refuse a missing asset")
        });

        assert!(matches!(error, UpdateError::MissingAsset(_)), "{error}");
        assert_eq!(std::fs::read(&exe).expect("read the exe"), b"old");
    }

    #[test]
    fn apply_refuses_a_release_without_a_checksum_file() {
        let name = asset_name(env!("HANDFAST_TARGET"));
        let source = fake("v99.0.0", &name, b"new", None);
        let (_directory, exe) = test_exe();
        let error = with_updater(&source, &Verifier(Ok(())), &exe, |updater| {
            updater
                .apply(&exe)
                .expect_err("refuse a missing checksum file")
        });

        assert!(matches!(error, UpdateError::MissingAsset(_)), "{error}");
        assert_eq!(std::fs::read(&exe).expect("read the exe"), b"old");
    }
}
