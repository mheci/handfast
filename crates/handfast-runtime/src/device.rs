//! Per-device runtime state.

use handfast_proto::{DeviceId, DeviceName, DeviceType};

/// A known device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Stable identifier.
    pub id: DeviceId,
    /// Display name.
    pub name: DeviceName,
    /// Type hint.
    pub device_type: DeviceType,
    /// Whether pairing has completed.
    pub paired: bool,
    /// Whether a link is currently open.
    pub connected: bool,
}

impl Device {
    /// Build an unpaired, disconnected device.
    #[must_use]
    pub fn new(id: DeviceId, name: DeviceName, device_type: DeviceType) -> Self {
        Self {
            id,
            name,
            device_type,
            paired: false,
            connected: false,
        }
    }
}
