//! Plugin runtime for Handfast.
//!
//! The runtime owns the per-device plugin set, dispatches decoded packets and
//! isolates plugin failures. It knows nothing about sockets or concrete
//! plugins.

#![deny(missing_docs)]

pub mod device;
pub mod error;
pub mod event;
pub mod host;
pub mod plugin;

pub use device::Device;
pub use error::{PluginError, RuntimeError};
pub use event::{BatteryState, Event, LogLevel, PluginEvent};
pub use host::{DeviceHost, Dispatch};
pub use plugin::{
    DesktopRequirement, Plugin, PluginContext, PluginFactory, PluginMeta, PluginOutput,
    PluginStatus, Registry,
};
