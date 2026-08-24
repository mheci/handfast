//! SFTP exposure: process lifecycle for OpenSSH's built-in SFTP server.
//!
//! When a paired phone asks for `kdeconnect.sftp`, the desktop side launches
//! `/usr/lib/openssh/sftp-server` (or the distro equivalent) with its stdio
//! piped, and tells the phone which local endpoint serves it. Upstream KDE
//! Connect multiplexes that stdio over the existing KDE Connect connection
//! instead of running a real sshd; the byte-level plumbing arrives in Phase 4.
//! This module owns everything Phase 3 needs:
//!
//! * resolving the `sftp-server` binary across common distro layouts,
//! * spawning one subprocess per requesting device (read-only `$HOME`),
//! * tearing it down again when the device disconnects or unmounts.
//!
//! # Safety posture
//!
//! Sessions serve `$HOME` with sftp-server's `-R` (read-only) flag — browsing
//! is the use case; writes belong to dedicated transfer packets. Children are
//! created with [`Command::kill_on_drop`] so a lost manager can never leak a
//! live fileserver, and teardown reaps asynchronously so callers never block
//! on an unkillable process.

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use handfast_core::error::{Error, Result};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

/// Environment variable overriding the resolved `sftp-server` binary.
///
/// Intended for packaging quirks and for tests that inject a mock binary;
/// takes precedence over every built-in search location.
pub const SFTP_SERVER_OVERRIDE_ENV: &str = "HANDFAST_SFTP_SERVER";

/// How long teardown waits for the child to be reaped before giving up.
/// After SIGKILL this only guards against a stuck runtime; on timeout the
/// `Child` handle is simply dropped (`kill_on_drop` remains the backstop).
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Common filesystem locations of OpenSSH's `sftp-server`, in the order
/// upstream KDE Connect probes them (Debian layout first, then Arch, Fedora,
/// Alpine/openSUSE and BSD/macOS layouts).
const SFTP_SERVER_CANDIDATES: &[&str] = &[
    "/usr/lib/openssh/sftp-server",
    "/usr/lib/ssh/sftp-server",
    "/usr/libexec/openssh/sftp-server",
    "/usr/lib/sftp-server",
    "/usr/libexec/sftp-server",
    "/usr/sbin/sftp-server",
];

/// Per-device SFTP subprocess bookkeeping.
struct SftpSession {
    /// Piped `sftp-server` process; Phase 4 splices its stdin/stdout onto the
    /// device's transport. Killed and reaped by [`SftpManager::shutdown_session`].
    child: Child,
    /// Local port the phone should connect to once the SSH transport exists.
    port: u16,
}

/// Owns the `sftp-server` subprocesses backing `kdeconnect.sftp` responses.
///
/// One instance lives alongside the device manager actor; every method is
/// infallible except [`SftpManager::start_session`], which reports failures
/// (most commonly a missing binary) as descriptive errors instead of panicking.
pub struct SftpManager {
    active_sessions: HashMap<String, SftpSession>,
}

impl SftpManager {
    pub fn new() -> Self {
        Self {
            active_sessions: HashMap::new(),
        }
    }

    /// Launch an `sftp-server` subprocess for `device_id`, serving `$HOME`
    /// read-only, and record `port` as the endpoint advertised back to the
    /// phone.
    ///
    /// An already-running session for the device is torn down first, so
    /// repeated `kdeconnect.sftp` requests simply refresh the mount. Returns
    /// a descriptive error when no usable `sftp-server` binary exists or the
    /// child dies immediately.
    pub async fn start_session(&mut self, device_id: &str, port: u16) -> Result<()> {
        if port == 0 {
            return Err(Error::Other(
                "refusing to start sftp session on port 0; the listener must bind first".into(),
            ));
        }

        // Refresh semantics: an existing mount is replaced, never doubled.
        if let Some(old) = self.active_sessions.remove(device_id) {
            debug!(device = %device_id, "replacing active sftp session");
            Self::shutdown_session(old);
        }

        let binary = locate_server_binary()?;
        let mut command = Command::new(&binary);
        command
            // -R: whole export is read-only; phones browse, never mutate.
            .arg("-R")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Last-resort cleanup if the manager dies without stop_session().
            .kill_on_drop(true);
        match home_dir() {
            Some(home) => {
                command.arg("-d").arg(home);
            }
            None => debug!(
                "no HOME/USERPROFILE in environment; sftp-server will expose its working directory"
            ),
        }

        let mut child = command.spawn().map_err(|err| {
            Error::Other(format!(
                "sftp server '{}' failed to spawn: {err}",
                binary.display()
            ))
        })?;
        // Catch "spawned but instantly dead" (bad interpreter, exec format,
        // permissions) now, while we can still blame the binary by name.
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                return Err(Error::Other(format!(
                    "sftp server '{}' exited immediately ({status})",
                    binary.display()
                )));
            }
            Err(err) => return Err(Error::Io(err)),
        }

        info!(
            device = %device_id,
            port,
            binary = %binary.display(),
            "sftp session started"
        );
        self.active_sessions
            .insert(device_id.to_string(), SftpSession { child, port });
        Ok(())
    }

    /// Kill and reap the device's subprocess, if any. Unknown devices are a
    /// quiet no-op so disconnect handlers can call this unconditionally.
    pub fn stop_session(&mut self, device_id: &str) {
        if let Some(session) = self.active_sessions.remove(device_id) {
            info!(device = %device_id, port = session.port, "stopping sftp session");
            Self::shutdown_session(session);
        }
    }

    /// Whether a live `sftp-server` subprocess exists for `device_id`.
    pub fn is_active(&self, device_id: &str) -> bool {
        self.active_sessions.contains_key(device_id)
    }

    /// The advertised endpoint port for `device_id`, if a session is running.
    pub fn session_port(&self, device_id: &str) -> Option<u16> {
        self.active_sessions.get(device_id).map(|s| s.port)
    }

    /// SIGKILL the child, then hand it to the runtime for asynchronous
    /// reaping; outside a runtime the `kill_on_drop` backstop applies.
    ///
    /// Sync on purpose: teardown must work from plain disconnect paths and
    /// must never block its caller on a wedged process.
    fn shutdown_session(mut session: SftpSession) {
        if let Err(err) = session.child.start_kill() {
            // Already-exited is the expected benign case; anything else is
            // logged but never fatal — the drop path retries the kill.
            debug!(%err, "sftp server kill returned an error");
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _ = tokio::time::timeout(REAP_TIMEOUT, session.child.wait()).await;
                });
            }
            Err(_) => {
                // No runtime: dropping still triggers kill_on_drop; final
                // reaping falls to the OS (init adopts the orphan).
                warn!("no tokio runtime for sftp reaping; relying on kill-on-drop");
            }
        }
    }
}

impl Default for SftpManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the `sftp-server` executable: `$HANDFAST_SFTP_SERVER` override,
/// then well-known absolute locations, then `$PATH`.
fn locate_server_binary() -> Result<PathBuf> {
    if let Some(overridden) = env::var_os(SFTP_SERVER_OVERRIDE_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(overridden));
    }
    for candidate in SFTP_SERVER_CANDIDATES {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    if let Some(from_path) = find_in_path("sftp-server") {
        return Ok(from_path);
    }
    Err(Error::Other(format!(
        "sftp server binary not found; install the package providing sftp-server \
         (e.g. openssh-sftp-server) searched at [{}], or point ${SFTP_SERVER_OVERRIDE_ENV} \
         at the binary",
        SFTP_SERVER_CANDIDATES.join(", ")
    )))
}

/// Search `$PATH` for `binary_name`, mirroring shell lookup semantics.
fn find_in_path(binary_name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|dir| dir.join(binary_name))
        .find(|candidate| candidate.is_file())
}

/// Home directory to export, preferring `HOME` and falling back to
/// `USERPROFILE` for non-unix development environments.
fn home_dir() -> Option<OsString> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
}

/// Sanity-check helper used by tests to build an executable stub.
#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
fn write_executable(path: &std::path::Path, contents: &str) {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let mut file = fs::File::create(path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o755))
        .unwrap();
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;

    /// Serializes every test that mutates the process-wide override variable.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_fixture_dir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("handfast-sftp-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn current_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn rejects_zero_port_without_spawning() {
        let mut manager = SftpManager::new();
        let err = manager.start_session("dev", 0).await.unwrap_err();
        assert!(err.to_string().contains("port 0"));
        assert!(!manager.is_active("dev"));
    }

    #[test]
    fn missing_binary_yields_descriptive_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        let bogus = PathBuf::from("/nonexistent/handfast/mock-sftp-server");
        env::set_var(SFTP_SERVER_OVERRIDE_ENV, &bogus);

        let result = current_thread_runtime().block_on(async {
            let mut manager = SftpManager::new();
            manager.start_session("dev", 1022).await
        });

        env::remove_var(SFTP_SERVER_OVERRIDE_ENV);
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("sftp"), "unexpected message: {msg}");
        assert!(
            msg.contains(bogus.to_str().unwrap()),
            "error should name the failing binary: {msg}"
        );
    }

    #[test]
    fn lifecycle_covers_start_restart_and_stop_transitions() {
        let _guard = ENV_LOCK.lock().unwrap();
        let fixture = temp_fixture_dir("lifecycle");
        let mock = fixture.join("mock-sftp-server");
        // Stub stays alive long enough for the manager to observe it running.
        write_executable(&mock, "#!/bin/sh\nexec sleep 60\n");
        env::set_var(SFTP_SERVER_OVERRIDE_ENV, &mock);

        current_thread_runtime().block_on(async {
            let mut manager = SftpManager::new();
            assert!(!manager.is_active("phone-a"));

            manager.start_session("phone-a", 2222).await.unwrap();
            assert!(manager.is_active("phone-a"));
            assert_eq!(manager.session_port("phone-a"), Some(2222));

            // A second request refreshes instead of doubling the process.
            manager.start_session("phone-a", 3333).await.unwrap();
            assert!(manager.is_active("phone-a"));
            assert_eq!(manager.session_port("phone-a"), Some(3333));

            // Sessions are tracked independently per device.
            manager.start_session("phone-b", 4444).await.unwrap();
            assert!(manager.is_active("phone-b"));

            manager.stop_session("phone-a");
            assert!(!manager.is_active("phone-a"));
            assert!(manager.is_active("phone-b"));

            manager.stop_session("phone-b");
            assert!(!manager.is_active("phone-b"));

            // Stopping an unknown device is a no-op.
            manager.stop_session("ghost");
        });

        env::remove_var(SFTP_SERVER_OVERRIDE_ENV);
        let _ignored = fs::remove_dir_all(&fixture);
    }
}
