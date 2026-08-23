//! Placeholder plugins for every roster entry not yet implemented.
//!
//! Each stub accepts its inbound packet types, logs them at `debug` level,
//! and emits no replies. They exist so the registry, supervision plumbing,
//! and per-plugin enable/disable config are exercised end-to-end before the
//! Phase-3 IO backends land.

use handfast_protocol::Packet;

use crate::{Plugin, PluginFactory, PluginMeta};

// TODO(phase3): implement
/// Generic placeholder plugin; inert beyond debug logging.
#[derive(Debug)]
pub struct StubPlugin {
    meta: &'static PluginMeta,
}

impl StubPlugin {
    /// Create a stub bound to the given compile-time metadata.
    #[must_use]
    pub const fn new(meta: &'static PluginMeta) -> Self {
        Self { meta }
    }
}

impl Plugin for StubPlugin {
    fn meta(&self) -> &'static PluginMeta {
        self.meta
    }

    fn handle(&mut self, pkt: &Packet) -> Vec<Packet> {
        tracing::debug!(
            plugin = self.meta.name,
            got = pkt.ty(),
            "stub plugin received packet"
        );
        Vec::new()
    }
}

macro_rules! stub_factory {
    ($(#[$doc:meta])* $factory:ident, $meta_const:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, Default)]
        pub struct $factory;

        impl PluginFactory for $factory {
            fn meta(&self) -> &'static PluginMeta {
                &$crate::meta::$meta_const
            }

            fn create(&self) -> Box<dyn Plugin> {
                Box::new(StubPlugin::new(&$crate::meta::$meta_const))
            }
        }
    };
}

stub_factory!(
    /// Builds battery placeholder instances.
    BatteryFactory,
    BATTERY
);
stub_factory!(
    /// Builds clipboard placeholder instances.
    ClipboardFactory,
    CLIPBOARD
);
stub_factory!(
    /// Builds connectivity-report placeholder instances.
    ConnectivityReportFactory,
    CONNECTIVITY_REPORT
);
stub_factory!(
    /// Builds contacts placeholder instances.
    ContactsFactory,
    CONTACTS
);
stub_factory!(
    /// Builds find-my-phone placeholder instances.
    FindMyPhoneFactory,
    FINDMYPHONE
);
stub_factory!(
    /// Builds mousepad placeholder instances.
    MousepadFactory,
    MOUSEPAD
);
stub_factory!(
    /// Builds MPRIS placeholder instances.
    MprisFactory,
    MPRIS
);
stub_factory!(
    /// Builds notifications placeholder instances.
    NotificationsFactory,
    NOTIFICATIONS
);
stub_factory!(
    /// Builds pause-music placeholder instances.
    PauseMusicFactory,
    PAUSE_MUSIC
);
stub_factory!(
    /// Builds run-commands placeholder instances.
    RunCommandsFactory,
    RUN_COMMANDS
);
stub_factory!(
    /// Builds remote-filesystem placeholder instances.
    RemoteFilesystemFactory,
    REMOTE_FILESYSTEM
);
stub_factory!(
    /// Builds share placeholder instances.
    ShareFactory,
    SHARE
);
stub_factory!(
    /// Builds SMS placeholder instances.
    SmsFactory,
    SMS
);
stub_factory!(
    /// Builds system-volume placeholder instances.
    SystemVolumeFactory,
    SYSTEM_VOLUME
);
stub_factory!(
    /// Builds telephony placeholder instances.
    TelephonyFactory,
    TELEPHONY
);
stub_factory!(
    /// Builds virtual-input placeholder instances.
    VirtualInputFactory,
    VIRTUAL_INPUT
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{meta, registry};
    use handfast_protocol::TYPE_PING;
    use serde_json::{json, Value};

    fn garbage_bodies() -> Vec<Value> {
        vec![
            Value::Null,
            json!(true),
            json!(false),
            json!(0),
            json!(-1),
            json!(u64::MAX),
            json!(i64::MIN),
            json!(1.5),
            json!(""),
            json!("not an object"),
            json!([]),
            json!(["mixed", 1, null]),
            json!({}),
            json!({"body": null}),
            json!({"body": {"nested": {"deep": [true, false, {"k": []}]}}}),
            json!({"payload": {"type": 42}}),
            json!({"key": "\u{0}\n\r\t\""}),
        ]
    }

    #[test]
    fn stubs_return_empty_and_never_panic_on_garbage() {
        let bodies = garbage_bodies();
        let mut types: Vec<&str> = meta::ALL
            .iter()
            .flat_map(|m| m.incoming.iter().copied())
            .collect();
        types.sort_unstable();
        types.dedup();

        for factory in registry() {
            let name = factory.meta().name;
            if name == meta::PING.name {
                continue;
            }
            let mut plugin = factory.create();
            for ty in &types {
                for body in &bodies {
                    let replies = plugin.handle(&Packet::new(ty, body.clone()));
                    assert!(replies.is_empty(), "stub {} replied to {}", name, ty);
                }
            }
        }
    }

    #[test]
    fn stub_metas_match_bound_constants() {
        assert_eq!(BatteryFactory.meta().name, "battery");
        assert_eq!(VirtualInputFactory.meta().name, "virtual_input");
        let mut p = StubPlugin::new(&meta::TELEPHONY);
        assert_eq!(p.meta().name, "telephony");
        assert!(
            p.handle(&Packet::new(TYPE_PING, Value::Null)).is_empty(),
            "stub must ignore foreign types too"
        );
    }

    #[test]
    fn sixteen_stub_factories_cover_non_ping_roster() {
        // Heterogeneous factory structs cannot share one array type; exercise
        // the public registry and filter out the fully-implemented ping.
        let stubs: Vec<Box<dyn PluginFactory>> = crate::registry()
            .into_iter()
            .filter(|factory| factory.meta().name != meta::PING.name)
            .collect();
        assert_eq!(stubs.len(), 16);
        let mut names: Vec<&str> = stubs.iter().map(|factory| factory.meta().name).collect();
        names.sort_unstable();
        let before_dedup = names.len();
        names.dedup();
        assert_eq!(names.len(), before_dedup, "roster names must be unique");
        assert!(!names.contains(&meta::PING.name));
    }
}
