//! Peer and process identity helpers for local access control.
//!
//! The workspace denies `unsafe` code, so instead of raw libc FFI this module
//! builds on two safe primitives:
//!
//! * `tokio::net::UnixStream::peer_cred()` — the safe wrapper over
//!   `SO_PEERCRED` used to learn the connecting peer's uid.
//! * `/proc/self/status` parsing (Linux) to learn our own effective uid.
//!
//! Only compiled on Unix; other platforms have no Unix domain sockets here.

#[cfg(unix)]
use tokio::net::UnixStream;

/// Effective uid of this process.
///
/// Linux reads the second field of the `Uid:` line in `/proc/self/status`
/// (`<real> <effective> <saved> <fs>`); other Unix variants have no portable
/// safe probe and report `None`.
#[cfg(target_os = "linux")]
pub(super) fn current_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("Uid:") else {
            continue;
        };
        return rest.split_whitespace().nth(1)?.parse::<u32>().ok();
    }
    None
}

/// Non-Linux Unix hosts: no portable safe uid probe; treat as unknown.
#[cfg(all(unix, not(target_os = "linux")))]
pub(super) fn current_uid() -> Option<u32> {
    None
}

/// Check that a freshly accepted client runs as the daemon's own user.
///
/// On Linux the peer's uid (via `SO_PEERCRED`) must equal our effective uid;
/// anything else — including undeterminable credentials — fails closed. On
/// non-Linux Unix there is no portable credential probe, so connections are
/// trusted (documented limitation; socket directory permissions are the
/// remaining barrier).
#[cfg(target_os = "linux")]
pub(super) fn peer_is_owner(stream: &UnixStream) -> bool {
    match stream.peer_cred() {
        Ok(cred) => match current_uid() {
            Some(euid) => {
                if cred.uid == euid {
                    true
                } else {
                    tracing::warn!(
                        target: "handfast::ipc",
                        peer_uid = cred.uid,
                        euid,
                        "rejected IPC client with foreign uid"
                    );
                    false
                }
            }
            None => {
                tracing::warn!(
                    target: "handfast::ipc",
                    "cannot determine own euid; rejecting IPC client"
                );
                false
            }
        },
        Err(err) => {
            tracing::debug!(
                target: "handfast::ipc",
                %err,
                "SO_PEERCRED unavailable; rejecting IPC client"
            );
            false
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(super) fn peer_is_owner(_stream: &UnixStream) -> bool {
    true
}
