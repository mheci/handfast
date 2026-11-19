//! Find my phone, and the phone-side "find this device" acknowledgement.
//!
//! `findmyphone` is the simplest plugin in the roster: it sends an empty
//! `kdeconnect.findmyphone.request` and the phone rings. There is no reply to
//! parse, no state to keep, and no desktop adapter involved.
//!
//! Handset requests arrive as the same packet type and are handled by
//! [`crate::findthisdevice`], which owns the desktop side. This plugin is the
//! outbound half only, matching upstream where `findmyphone` advertises no
//! incoming type.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta, PluginOutput,
};

use crate::roster;

/// Requests the peer ring itself.
#[derive(Debug, Default)]
pub struct FindMyPhonePlugin {
    /// Requests queued for the next tick.
    pending: usize,
}

impl FindMyPhonePlugin {
    /// Create a find my phone plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self { pending: 0 }
    }

    /// Queue a ring request.
    ///
    /// Queued rather than sent directly for the same reason the other
    /// desktop-initiated plugins do it: a UI or control channel can call this
    /// from any thread, and the session owns the outgoing packet channel.
    pub fn ring(&mut self) {
        self.pending = self.pending.saturating_add(1);
    }

    /// Number of queued requests.
    #[must_use]
    pub const fn pending(&self) -> usize {
        self.pending
    }
}

#[async_trait]
impl Plugin for FindMyPhonePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::FINDMYPHONE
    }

    async fn on_tick(&mut self, _ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        if self.pending == 0 {
            return Ok(PluginOutput::new());
        }
        let rings = self.pending;
        self.pending = 0;
        let mut output = PluginOutput::new();
        for _ in 0..rings {
            output = output.with_packet(
                Packet::new(types::FINDMYPHONE_REQUEST, serde_json::json!({}))
                    .map_err(|error| PluginError::InvalidPacket(error.to_string()))?,
            );
        }
        Ok(output.with_event(PluginEvent::Log {
            level: handfast_runtime::LogLevel::Info,
            message: format!("asked the peer to ring {rings} time(s)"),
        }))
    }
}

/// Builds [`FindMyPhonePlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct FindMyPhoneFactory;

impl PluginFactory for FindMyPhoneFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::FINDMYPHONE
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(FindMyPhonePlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginConfig, PluginStatus};

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Phone"),
            DeviceType::Phone,
        )
    }

    async fn tick(plugin: &mut FindMyPhonePlugin) -> PluginOutput {
        let desktop = MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        plugin.on_tick(&mut ctx).await.unwrap()
    }

    #[test]
    fn the_roster_entry_is_outbound_only() {
        assert_eq!(roster::FINDMYPHONE.status, PluginStatus::Implemented);
        assert!(roster::FINDMYPHONE.incoming.is_empty());
        assert_eq!(roster::FINDMYPHONE.outgoing, &[types::FINDMYPHONE_REQUEST]);
    }

    #[tokio::test]
    async fn a_queued_ring_leaves_on_the_next_tick() {
        let mut plugin = FindMyPhonePlugin::new();
        assert!(tick(&mut plugin).await.packets.is_empty());

        plugin.ring();
        plugin.ring();
        assert_eq!(plugin.pending(), 2);

        let output = tick(&mut plugin).await;
        assert_eq!(plugin.pending(), 0);
        assert_eq!(output.packets.len(), 2);
        assert!(output
            .packets
            .iter()
            .all(|packet| packet.ptype == types::FINDMYPHONE_REQUEST));
        // The request body is empty upstream; a payload-free packet is the
        // whole wire format.
        assert!(output.packets[0].body.is_empty());
    }
}
