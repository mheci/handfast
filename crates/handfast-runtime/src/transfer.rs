//! Transfer intents returned by plugins.
//!
//! A plugin describes the transfer and the daemon session executes it. The
//! runtime never opens a socket or a file.

use handfast_proto::{PayloadOffer, PayloadSize};

/// A file transfer the session must run outside the plugin call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transfer {
    /// Receive one file from the peer.
    ReceiveFile {
        /// Peer-supplied file name. The Files adapter treats it as an
        /// untrusted basename.
        filename: Option<String>,
        /// Declared payload size.
        size: PayloadSize,
        /// Validated size and port when the file has bytes. `None` for an
        /// empty file, which has no payload connection.
        offer: Option<PayloadOffer>,
    },
}
