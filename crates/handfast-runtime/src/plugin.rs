//! Plugin contracts and registry.

use async_trait::async_trait;
use handfast_desktop::Desktop;
use handfast_proto::{Capabilities, Packet};

use crate::device::Device;
use crate::error::{PluginError, RuntimeError};
use crate::event::PluginEvent;
use crate::transfer::Transfer;

/// Lifecycle state of a plugin in the v2 roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStatus {
    /// Packet contract and host integration are implemented.
    Implemented,
    /// Listed in the parity matrix but not advertised or dispatched yet.
    Planned,
}

/// Desktop capability a plugin depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopRequirement {
    /// No desktop adapter.
    None,
    /// Clipboard.
    Clipboard,
    /// Notifications.
    Notifications,
    /// MPRIS media control.
    Media,
    /// System volume.
    Volume,
    /// Pointer or keyboard injection.
    Input,
    /// Screensaver inhibition.
    Screensaver,
    /// URL or file opening.
    Shell,
    /// User files or SFTP.
    Files,
    /// Telephony or SMS.
    Telephony,
    /// Contacts storage.
    Contacts,
    /// Virtual display output.
    Display,
}

/// Static plugin metadata. This is the single source of truth for capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginMeta {
    /// Canonical plugin name, for example `clipboard`.
    pub name: &'static str,
    /// Human-readable title.
    pub title: &'static str,
    /// Implementation status.
    pub status: PluginStatus,
    /// Packet types consumed by the plugin.
    pub incoming: &'static [&'static str],
    /// Packet types emitted by the plugin.
    pub outgoing: &'static [&'static str],
    /// Required desktop adapter.
    pub desktop: DesktopRequirement,
}

/// Packets, events and transfers produced by one plugin invocation.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PluginOutput {
    /// Outbound packets.
    pub packets: Vec<Packet>,
    /// Typed events.
    pub events: Vec<PluginEvent>,
    /// Transfer intents the session must run.
    pub transfers: Vec<Transfer>,
}

impl PluginOutput {
    /// Empty output.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an outbound packet.
    #[must_use]
    pub fn with_packet(mut self, packet: Packet) -> Self {
        self.packets.push(packet);
        self
    }

    /// Append an event.
    #[must_use]
    pub fn with_event(mut self, event: PluginEvent) -> Self {
        self.events.push(event);
        self
    }

    /// Append a transfer intent.
    #[must_use]
    pub fn with_transfer(mut self, transfer: Transfer) -> Self {
        self.transfers.push(transfer);
        self
    }
}

/// Borrowed context available to a plugin invocation.
pub struct PluginContext<'a> {
    /// Device that sent or will receive the packet.
    pub device: &'a Device,
    /// Desktop adapters on this host.
    pub desktop: &'a Desktop,
}

/// A fault-isolated packet handler.
#[async_trait]
pub trait Plugin: Send + Sync {
    /// Static metadata.
    fn meta(&self) -> &'static PluginMeta;

    /// Called when a device link opens.
    async fn on_connect(
        &mut self,
        _ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        Ok(PluginOutput::new())
    }

    /// Handle one inbound packet.
    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError>;

    /// Called when a device link closes.
    async fn on_disconnect(
        &mut self,
        _ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        Ok(PluginOutput::new())
    }
}

/// Creates fresh plugin instances.
pub trait PluginFactory: Send + Sync {
    /// Metadata for the produced plugin.
    fn meta(&self) -> &'static PluginMeta;

    /// Build a new instance.
    fn create(&self) -> Box<dyn Plugin>;
}

/// All plugin factories known to the daemon.
pub struct Registry {
    factories: Vec<Box<dyn PluginFactory>>,
}

impl Registry {
    /// Build a registry from factories.
    #[must_use]
    pub fn new(factories: Vec<Box<dyn PluginFactory>>) -> Self {
        Self { factories }
    }

    /// Registered factories.
    #[must_use]
    pub fn factories(&self) -> &[Box<dyn PluginFactory>] {
        &self.factories
    }

    /// Metadata for every registered plugin.
    pub fn metas(&self) -> impl Iterator<Item = &'static PluginMeta> + '_ {
        self.factories.iter().map(|factory| factory.meta())
    }

    /// Create one instance of every registered plugin.
    #[must_use]
    pub fn create_all(&self) -> Vec<Box<dyn Plugin>> {
        self.factories
            .iter()
            .map(|factory| factory.create())
            .collect()
    }

    /// Advertised capabilities from implemented plugins whose desktop
    /// adapters exist on this host.
    ///
    /// # Errors
    ///
    /// Fails if a plugin metadata table contains a malformed packet type.
    pub fn capabilities(&self, desktop: &Desktop) -> Result<Capabilities, RuntimeError> {
        let mut incoming = Vec::new();
        let mut outgoing = Vec::new();
        for factory in &self.factories {
            let meta = factory.meta();
            if meta.status != PluginStatus::Implemented || !desktop_available(desktop, meta.desktop)
            {
                continue;
            }
            incoming.extend(meta.incoming.iter().copied());
            outgoing.extend(meta.outgoing.iter().copied());
        }
        Ok(Capabilities::new(incoming, outgoing)?)
    }
}

pub(crate) fn desktop_available(desktop: &Desktop, requirement: DesktopRequirement) -> bool {
    match requirement {
        DesktopRequirement::None => true,
        DesktopRequirement::Clipboard => desktop.clipboard.is_some(),
        DesktopRequirement::Notifications => desktop.notifier.is_some(),
        DesktopRequirement::Media => desktop.media.is_some(),
        DesktopRequirement::Volume => desktop.volume.is_some(),
        DesktopRequirement::Input => desktop.input.is_some(),
        DesktopRequirement::Screensaver => desktop.screensaver.is_some(),
        DesktopRequirement::Shell => desktop.shell.is_some(),
        DesktopRequirement::Files => desktop.files.is_some(),
        DesktopRequirement::Telephony
        | DesktopRequirement::Contacts
        | DesktopRequirement::Display => false,
    }
}
