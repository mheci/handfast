//! Find my phone plugin.

use async_trait::async_trait;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    Plugin, PluginContext, PluginError, PluginFactory, PluginMeta, PluginOutput,
};

use crate::roster;

/// Advertises the find-my-phone capability; the control channel sends the
/// request.
#[derive(Debug, Default)]
pub struct FindMyPhonePlugin;

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
        Box::new(FindMyPhonePlugin)
    }
}

/// Build the empty ring request packet.
///
/// # Errors
///
/// Fails when the packet type is malformed.
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
