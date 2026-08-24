#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Handfast local IPC.
//!
//! Typed JSON communication between the daemon and local UI clients (TUI,
//! GUI, CLI) over a Unix domain socket.
//!
//! # Wire format
//!
//! Every message — request, response or server event — is one *frame*:
//! a little-endian `u32` payload length followed by that many bytes of JSON
//! (see [`codec`]). Payloads larger than [`MAX_FRAME_BYTES`] are rejected.
//!
//! # Protocol
//!
//! A client connects, immediately receives a
//! [`ServerEvent::Hello`] handshake, then exchanges [`Request`]/[`Response`]
//! pairs while the server independently pushes [`ServerEvent`] broadcasts.
//! On Linux the daemon verifies the peer's uid via `SO_PEERCRED` before
//! serving it; connections from other users are closed.
//!
//! Windows has no Unix sockets; [`default_socket_path`] documents the named
//! pipe placeholder and [`Server::bind`]/[`Client::connect`] return an error
//! until named-pipe support lands. All types still compile there so the rest
//! of the workspace typechecks unchanged.

#![forbid(unsafe_code)]

mod client;
pub mod codec;
pub mod error;
mod peercred;
mod proto;
mod server;

pub use client::Client;
pub use error::{Error, Result};
pub use proto::{Request, Response, ServerEvent};
pub use server::{RequestHandler, Server};

/// Wire protocol version reported in [`ServerEvent::Hello`].
pub const IPC_VERSION: u32 = 1;

/// Maximum accepted frame payload size (16 MiB).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Default location of the daemon's IPC endpoint.
///
/// * Unix: `$XDG_RUNTIME_DIR/handfast/handfast.sock`, else `/tmp/handfast-{uid}.sock`
///   (Linux; other Unix falls back to `/tmp/handfast.sock`). The `handfast`
///   subdir is provisioned by systemd `RuntimeDirectory=handfast` when running
///   under the packaged `handfast.service` (see `packaging/systemd/handfast.service`);
///   bare `XDG_RUNTIME_DIR` execution still works because the path is created
///   on demand by the daemon.
/// * Windows: `\\.\pipe\handfast` — a documented stub for future named-pipe
///   transport support.
#[must_use]
pub fn default_socket_path() -> std::path::PathBuf {
    #[cfg(unix)]
    {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
            return std::path::PathBuf::from(dir)
                .join(handfast_core::APP_NAME)
                .join(format!("{}.sock", handfast_core::APP_NAME));
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(uid) = peercred::current_uid() {
                return std::path::PathBuf::from("/tmp")
                    .join(format!("{}-{uid}.sock", handfast_core::APP_NAME));
            }
        }
        std::path::PathBuf::from("/tmp").join(format!("{}.sock", handfast_core::APP_NAME))
    }

    #[cfg(windows)]
    {
        std::path::PathBuf::from(r"\\.\pipe\handfast")
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        std::env::temp_dir().join(format!("{}.sock", handfast_core::APP_NAME))
    }
}
