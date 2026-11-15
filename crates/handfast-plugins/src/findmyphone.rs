//! Find my phone plugin.

use async_trait::async_trait;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    Plugin, PluginContext, PluginError, PluginFactory, PluginMeta, PluginOutput,
};

use crate::roster;

/// Rings a device on demand.
#[derive(Debug, Default)]
pub struct FindMyPhonePlugin;

impl FindMyPhonePlugin {
    /// Create a find-my-phone plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Plugin for FindMyPhonePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::FINDMYPHONE
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        _packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        Ok(PluginOutput::new())
    }
}

/// Builds find-my-phone plugin instances.
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

/// Build the empty ring request packet.
///
/// # Errors
///
/// Fails only if the packet body cannot be constructed.
pub fn findmyphone_packet() -> Result<Packet, ProtoError> {
    Packet::empty(types::FINDMYPHONE_REQUEST)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_packet_is_empty() {
        let packet = findmyphone_packet().unwrap();
        assert_eq!(packet.ptype, types::FINDMYPHONE_REQUEST);
        assert!(packet.body.is_empty());
    }
}
