//! Runtime and plugin errors.

use handfast_proto::ProtoError;
use thiserror::Error;

/// Errors a plugin may return.
#[derive(Debug, Error)]
pub enum PluginError {
    /// The plugin needs a desktop adapter that is not present.
    #[error("desktop capability unavailable: {0}")]
    UnsupportedDesktop(&'static str),

    /// A peer sent a packet the plugin cannot use.
    #[error("invalid packet: {0}")]
    InvalidPacket(String),

    /// A desktop or storage backend failed.
    #[error("backend error: {0}")]
    Backend(String),
}

/// Errors produced by the device host.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// A plugin panicked. It is disabled for this device.
    #[error("plugin {plugin} panicked")]
    PluginPanicked {
        /// Plugin name.
        plugin: &'static str,
    },

    /// A plugin exceeded its time budget. It is disabled for this device.
    #[error("plugin {plugin} timed out")]
    PluginTimedOut {
        /// Plugin name.
        plugin: &'static str,
    },

    /// A plugin returned an error. It is disabled for this device.
    #[error("plugin {plugin} failed: {source}")]
    PluginFailed {
        /// Plugin name.
        plugin: &'static str,
        /// Underlying cause.
        #[source]
        source: PluginError,
    },

    /// Protocol setup failed.
    #[error("protocol: {0}")]
    Protocol(#[from] ProtoError),
}
