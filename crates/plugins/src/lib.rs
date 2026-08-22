//! Handfast plugin registry and implementations.
//!
//! Phase-1 design: plugins are **pure packet transformers**. A plugin receives
//! one decoded [`Packet`] at a time and returns zero or more reply packets.
//! Network, desktop (Wayland/DBus), and timer IO arrive in Phase 3; until then
//! the [`Plugin`] trait is synchronous and side-effect free apart from
//! `tracing` output.
//!
//! # Fault isolation
//!
//! Panics raised inside [`Plugin::handle`] are caught by the daemon supervisor,
//! which tears down the plugin instance and recreates it from its factory with
//! exponential backoff. Plugins must therefore never rely on cross-call state
//! surviving a panic.

use handfast_protocol::Packet;

pub mod meta;
pub mod ping;
pub mod stubs;

pub use ping::{PingFactory, PingPlugin};

/// Static capability metadata for a plugin, known at compile time.
pub struct PluginMeta {
    /// Canonical identifier used in config files, e.g. `"battery"`.
    pub name: &'static str,
    /// Human-readable label for UI surfaces.
    pub title: &'static str,
    /// `kdeconnect.*` packet types this plugin handles when inbound.
    pub incoming: &'static [&'static str],
    /// `kdeconnect.*` packet types this plugin may emit outbound.
    pub outgoing: &'static [&'static str],
    /// Whether the plugin is enabled unless the user opts out.
    pub default_enabled: bool,
    /// True when the plugin needs the Wayland bridge (input, clipboard, idle).
    pub requires_wayland: bool,
    /// True when the plugin needs the desktop DBus session (MPRIS, notifies).
    pub requires_dbus: bool,
}

/// A fault-isolated packet handler.
///
/// Panics inside [`handle`](Plugin::handle) are caught by the daemon
/// supervisor and the plugin instance is recreated from its factory.
pub trait Plugin: Send {
    /// Compile-time metadata describing this plugin kind.
    fn meta(&self) -> &'static PluginMeta;
    /// Process one inbound packet; return zero or more outbound replies.
    fn handle(&mut self, pkt: &Packet) -> Vec<Packet>;
}

/// Creates fresh [`Plugin`] instances on demand.
///
/// The daemon supervisor calls [`create`](PluginFactory::create) at device
/// connect time and again whenever a plugin instance is restarted after a
/// panic.
pub trait PluginFactory: Send + Sync {
    /// Compile-time metadata for the plugins this factory builds.
    fn meta(&self) -> &'static PluginMeta;
    /// Build a brand-new, default-state plugin instance.
    fn create(&self) -> Box<dyn Plugin>;
}

/// All plugin factories in deterministic order (Phase-1 parity roster with
/// upstream kdeconnect-kde): battery, clipboard, connectivity_report, contacts,
/// findmyphone, mousepad, mpris, notifications, pause_music, ping,
/// run_commands, remote_filesystem, share, sms, system_volume, telephony,
/// virtual_input.
#[must_use]
pub fn registry() -> Vec<Box<dyn PluginFactory>> {
    vec![
        Box::new(stubs::BatteryFactory),
        Box::new(stubs::ClipboardFactory),
        Box::new(stubs::ConnectivityReportFactory),
        Box::new(stubs::ContactsFactory),
        Box::new(stubs::FindMyPhoneFactory),
        Box::new(stubs::MousepadFactory),
        Box::new(stubs::MprisFactory),
        Box::new(stubs::NotificationsFactory),
        Box::new(stubs::PauseMusicFactory),
        Box::new(ping::PingFactory),
        Box::new(stubs::RunCommandsFactory),
        Box::new(stubs::RemoteFilesystemFactory),
        Box::new(stubs::ShareFactory),
        Box::new(stubs::SmsFactory),
        Box::new(stubs::SystemVolumeFactory),
        Box::new(stubs::TelephonyFactory),
        Box::new(stubs::VirtualInputFactory),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_protocol::TYPE_PING;
    use std::collections::HashSet;

    #[test]
    fn registry_has_full_roster_in_deterministic_order() {
        let factories = registry();
        assert_eq!(factories.len(), meta::ALL.len());
        assert!(factories.len() >= 17);
        for (factory, expected) in factories.iter().zip(meta::ALL.iter()) {
            assert_eq!(factory.meta().name, expected.name);
        }
    }

    #[test]
    fn registry_names_are_unique() {
        let names: HashSet<&str> = registry().iter().map(|f| f.meta().name).collect();
        assert_eq!(names.len(), 17);
    }

    #[test]
    fn every_packet_type_is_kdeconnect_namespaced() {
        for m in meta::ALL {
            for ty in m.incoming.iter().chain(m.outgoing.iter()) {
                assert!(
                    ty.starts_with("kdeconnect."),
                    "{} exposes non-namespaced type {}",
                    m.name,
                    ty
                );
            }
        }
    }

    #[test]
    fn factories_build_instances_with_matching_meta() {
        for factory in registry() {
            let m = factory.meta();
            let mut plugin = factory.create();
            assert_eq!(plugin.meta().name, m.name);
            let replies = plugin.handle(&Packet::new(TYPE_PING, serde_json::json!({})));
            if m.name != "ping" {
                assert!(
                    replies.is_empty(),
                    "stub plugin {} unexpectedly replied",
                    m.name
                );
            }
        }
    }
}
