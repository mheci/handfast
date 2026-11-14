//! Per-device plugin host with panic isolation.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use handfast_desktop::Desktop;
use handfast_proto::Packet;

use crate::device::Device;
use crate::error::{PluginError, RuntimeError};
use crate::event::PluginEvent;
use crate::plugin::{
    desktop_available, Plugin, PluginContext, PluginOutput, PluginStatus, Registry,
};
use crate::transfer::Transfer;

/// Maximum time one plugin call may run.
const PLUGIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Result of one host operation.
#[derive(Debug, Default)]
pub struct Dispatch {
    /// Outbound packets from all plugins.
    pub packets: Vec<Packet>,
    /// Events from all plugins.
    pub events: Vec<PluginEvent>,
    /// Transfer intents from all plugins.
    pub transfers: Vec<Transfer>,
    /// Failures. Failed plugins are disabled for this device.
    pub errors: Vec<RuntimeError>,
}

impl Dispatch {
    fn absorb(&mut self, output: PluginOutput) {
        self.packets.extend(output.packets);
        self.events.extend(output.events);
        self.transfers.extend(output.transfers);
    }
}

enum Call<'a> {
    Connect,
    Packet(&'a Packet),
    Disconnect,
}

async fn invoke(
    plugin: &mut dyn Plugin,
    device: &Device,
    desktop: &Desktop,
    call: Call<'_>,
) -> Result<PluginOutput, RuntimeError> {
    let meta = plugin.meta();
    let mut ctx = PluginContext { device, desktop };
    let future: BoxFuture<'_, Result<PluginOutput, PluginError>> = match call {
        Call::Connect => Box::pin(plugin.on_connect(&mut ctx)),
        Call::Packet(packet) => Box::pin(plugin.on_packet(&mut ctx, packet)),
        Call::Disconnect => Box::pin(plugin.on_disconnect(&mut ctx)),
    };
    match tokio::time::timeout(PLUGIN_TIMEOUT, AssertUnwindSafe(future).catch_unwind()).await {
        Ok(Ok(Ok(output))) => Ok(output),
        Ok(Ok(Err(source))) => Err(RuntimeError::PluginFailed {
            plugin: meta.name,
            source,
        }),
        Ok(Err(_)) => Err(RuntimeError::PluginPanicked { plugin: meta.name }),
        Err(_) => Err(RuntimeError::PluginTimedOut { plugin: meta.name }),
    }
}

/// Owns the plugin instances for one device.
pub struct DeviceHost {
    device: Device,
    desktop: Desktop,
    plugins: Vec<Box<dyn Plugin>>,
    enabled: HashSet<&'static str>,
    failed: HashSet<&'static str>,
}

impl DeviceHost {
    /// Build a host from a registry.
    #[must_use]
    pub fn new(device: Device, registry: &Registry, desktop: Desktop) -> Self {
        let plugins = registry.create_all();
        let enabled = plugins
            .iter()
            .filter(|plugin| {
                plugin.meta().status == PluginStatus::Implemented
                    && desktop_available(&desktop, plugin.meta().desktop)
            })
            .map(|plugin| plugin.meta().name)
            .collect();
        Self {
            device,
            desktop,
            plugins,
            enabled,
            failed: HashSet::new(),
        }
    }

    /// Enable or disable a plugin. Returns `false` for unknown or planned
    /// plugins.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let Some(plugin) = self
            .plugins
            .iter()
            .find(|plugin| plugin.meta().name == name)
        else {
            return false;
        };
        let meta = plugin.meta();
        if meta.status != PluginStatus::Implemented {
            return false;
        }
        if enabled {
            self.enabled.insert(meta.name);
            self.failed.remove(meta.name);
        } else {
            self.enabled.remove(meta.name);
        }
        true
    }

    /// Whether a plugin is enabled.
    #[must_use]
    pub fn is_enabled(&self, name: &str) -> bool {
        self.enabled.contains(name)
    }

    /// Call `on_connect` on every enabled plugin.
    pub async fn on_connect(&mut self) -> Dispatch {
        let mut result = Dispatch::default();
        let mut newly_failed = Vec::new();
        let Self {
            device,
            desktop,
            plugins,
            enabled,
            failed,
        } = self;
        for plugin in plugins.iter_mut() {
            let meta = plugin.meta();
            if meta.status != PluginStatus::Implemented
                || !enabled.contains(meta.name)
                || failed.contains(meta.name)
            {
                continue;
            }
            match invoke(plugin.as_mut(), device, desktop, Call::Connect).await {
                Ok(output) => result.absorb(output),
                Err(error) => {
                    newly_failed.push(meta.name);
                    result.errors.push(error);
                }
            }
        }
        for name in newly_failed {
            failed.insert(name);
        }
        result
    }

    /// Dispatch one packet to every matching plugin.
    pub async fn dispatch(&mut self, packet: &Packet) -> Dispatch {
        let mut result = Dispatch::default();
        let mut newly_failed = Vec::new();
        let Self {
            device,
            desktop,
            plugins,
            enabled,
            failed,
        } = self;
        for plugin in plugins.iter_mut() {
            let meta = plugin.meta();
            if meta.status != PluginStatus::Implemented
                || !enabled.contains(meta.name)
                || failed.contains(meta.name)
            {
                continue;
            }
            if !meta.incoming.contains(&packet.ptype.as_str()) {
                continue;
            }
            match invoke(plugin.as_mut(), device, desktop, Call::Packet(packet)).await {
                Ok(output) => result.absorb(output),
                Err(error) => {
                    newly_failed.push(meta.name);
                    result.errors.push(error);
                }
            }
        }
        for name in newly_failed {
            failed.insert(name);
        }
        result
    }

    /// Call `on_disconnect` on every enabled plugin.
    pub async fn on_disconnect(&mut self) -> Dispatch {
        let mut result = Dispatch::default();
        let Self {
            device,
            desktop,
            plugins,
            enabled,
            failed,
        } = self;
        for plugin in plugins.iter_mut() {
            let meta = plugin.meta();
            if meta.status != PluginStatus::Implemented
                || !enabled.contains(meta.name)
                || failed.contains(meta.name)
            {
                continue;
            }
            match invoke(plugin.as_mut(), device, desktop, Call::Disconnect).await {
                Ok(output) => result.absorb(output),
                Err(error) => result.errors.push(error),
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PluginEvent;
    use crate::plugin::{
        DesktopRequirement, PluginFactory, PluginMeta, PluginOutput, PluginStatus,
    };
    use crate::transfer::Transfer;
    use async_trait::async_trait;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{types, DeviceId, DeviceName, DeviceType, PayloadOffer, PayloadSize};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn meta_with_requirement(
        name: &'static str,
        status: PluginStatus,
        incoming: &'static [&'static str],
        desktop: DesktopRequirement,
    ) -> &'static PluginMeta {
        Box::leak(Box::new(PluginMeta {
            name,
            title: name,
            status,
            incoming,
            outgoing: &[],
            desktop,
        }))
    }

    fn meta(
        name: &'static str,
        status: PluginStatus,
        incoming: &'static [&'static str],
    ) -> &'static PluginMeta {
        meta_with_requirement(name, status, incoming, DesktopRequirement::None)
    }

    struct TestPlugin {
        meta: &'static PluginMeta,
        calls: Arc<Mutex<Vec<String>>>,
        panic_on_packet: bool,
        hang_on_packet: bool,
    }

    #[async_trait]
    impl Plugin for TestPlugin {
        fn meta(&self) -> &'static PluginMeta {
            self.meta
        }

        async fn on_connect(
            &mut self,
            _ctx: &mut PluginContext<'_>,
        ) -> Result<PluginOutput, PluginError> {
            self.calls.lock().unwrap().push("connect".to_string());
            Ok(PluginOutput::new())
        }

        async fn on_packet(
            &mut self,
            _ctx: &mut PluginContext<'_>,
            packet: &Packet,
        ) -> Result<PluginOutput, PluginError> {
            self.calls.lock().unwrap().push(packet.ptype.clone());
            if self.panic_on_packet {
                panic!("test panic");
            }
            if self.hang_on_packet {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            Ok(PluginOutput::new().with_event(PluginEvent::Ping {
                message: Some("pong".to_string()),
            }))
        }

        async fn on_disconnect(
            &mut self,
            _ctx: &mut PluginContext<'_>,
        ) -> Result<PluginOutput, PluginError> {
            self.calls.lock().unwrap().push("disconnect".to_string());
            Ok(PluginOutput::new())
        }
    }

    struct TestFactory {
        meta: &'static PluginMeta,
        calls: Arc<Mutex<Vec<String>>>,
        panic_on_packet: bool,
        hang_on_packet: bool,
    }

    impl PluginFactory for TestFactory {
        fn meta(&self) -> &'static PluginMeta {
            self.meta
        }

        fn create(&self) -> Box<dyn Plugin> {
            Box::new(TestPlugin {
                meta: self.meta,
                calls: Arc::clone(&self.calls),
                panic_on_packet: self.panic_on_packet,
                hang_on_packet: self.hang_on_packet,
            })
        }
    }

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test device"),
            DeviceType::Desktop,
        )
    }

    fn host(factories: Vec<Box<dyn PluginFactory>>) -> DeviceHost {
        let registry = Registry::new(factories);
        DeviceHost::new(device(), &registry, MockDesktop::new().desktop())
    }

    fn ping() -> Packet {
        Packet::from_body(types::PING, &json!({"message": "hi"})).unwrap()
    }

    #[tokio::test]
    async fn dispatch_only_calls_matching_plugins() {
        let ping_calls = Arc::new(Mutex::new(Vec::new()));
        let battery_calls = Arc::new(Mutex::new(Vec::new()));
        let mut host = host(vec![
            Box::new(TestFactory {
                meta: meta("ping", PluginStatus::Implemented, &[types::PING]),
                calls: Arc::clone(&ping_calls),
                panic_on_packet: false,
                hang_on_packet: false,
            }),
            Box::new(TestFactory {
                meta: meta("battery", PluginStatus::Implemented, &[types::BATTERY]),
                calls: Arc::clone(&battery_calls),
                panic_on_packet: false,
                hang_on_packet: false,
            }),
        ]);

        let result = host.dispatch(&ping()).await;
        assert!(result.errors.is_empty());
        assert_eq!(result.events.len(), 1);
        assert_eq!(*ping_calls.lock().unwrap(), vec![types::PING.to_string()]);
        assert!(battery_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn planned_plugins_are_not_dispatched_or_advertised() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let registry = Registry::new(vec![Box::new(TestFactory {
            meta: meta("planned", PluginStatus::Planned, &[types::PING]),
            calls: Arc::clone(&calls),
            panic_on_packet: false,
            hang_on_packet: false,
        })]);
        let desktop = MockDesktop::new().desktop();
        let capabilities = registry.capabilities(&desktop).unwrap();
        assert!(!capabilities.accepts(types::PING));

        let mut host = DeviceHost::new(device(), &registry, MockDesktop::new().desktop());
        let result = host.dispatch(&ping()).await;
        assert!(result.errors.is_empty());
        assert!(calls.lock().unwrap().is_empty());
        assert!(!host.set_enabled("planned", true));
    }

    #[tokio::test]
    async fn enable_and_disable_plugins() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut host = host(vec![Box::new(TestFactory {
            meta: meta("ping", PluginStatus::Implemented, &[types::PING]),
            calls: Arc::clone(&calls),
            panic_on_packet: false,
            hang_on_packet: false,
        })]);
        assert!(host.is_enabled("ping"));
        assert!(host.set_enabled("ping", false));
        host.dispatch(&ping()).await;
        assert!(calls.lock().unwrap().is_empty());
        assert!(host.set_enabled("ping", true));
        host.dispatch(&ping()).await;
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(!host.set_enabled("missing", true));
    }

    #[tokio::test]
    async fn panicking_plugin_is_isolated_and_disabled() {
        let panic_calls = Arc::new(Mutex::new(Vec::new()));
        let good_calls = Arc::new(Mutex::new(Vec::new()));
        let mut host = host(vec![
            Box::new(TestFactory {
                meta: meta("panics", PluginStatus::Implemented, &[types::PING]),
                calls: Arc::clone(&panic_calls),
                panic_on_packet: true,
                hang_on_packet: false,
            }),
            Box::new(TestFactory {
                meta: meta("ping", PluginStatus::Implemented, &[types::PING]),
                calls: Arc::clone(&good_calls),
                panic_on_packet: false,
                hang_on_packet: false,
            }),
        ]);

        let first = host.dispatch(&ping()).await;
        assert_eq!(first.errors.len(), 1);
        assert_eq!(first.events.len(), 1);
        assert_eq!(panic_calls.lock().unwrap().len(), 1);
        assert_eq!(good_calls.lock().unwrap().len(), 1);

        let second = host.dispatch(&ping()).await;
        assert!(second.errors.is_empty());
        assert_eq!(panic_calls.lock().unwrap().len(), 1);
        assert_eq!(good_calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn lifecycle_calls_reach_enabled_plugins() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut host = host(vec![Box::new(TestFactory {
            meta: meta("ping", PluginStatus::Implemented, &[types::PING]),
            calls: Arc::clone(&calls),
            panic_on_packet: false,
            hang_on_packet: false,
        })]);
        host.on_connect().await;
        host.on_disconnect().await;
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["connect".to_string(), "disconnect".to_string()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_plugin_times_out_and_is_disabled() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut host = host(vec![Box::new(TestFactory {
            meta: meta("slow", PluginStatus::Implemented, &[types::PING]),
            calls: Arc::clone(&calls),
            panic_on_packet: false,
            hang_on_packet: true,
        })]);

        let first = host.dispatch(&ping()).await;
        assert_eq!(first.errors.len(), 1);
        assert!(matches!(
            &first.errors[0],
            RuntimeError::PluginTimedOut { plugin } if *plugin == "slow"
        ));

        let second = host.dispatch(&ping()).await;
        assert!(second.errors.is_empty());
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn capabilities_require_the_desktop_adapter() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let registry = Registry::new(vec![Box::new(TestFactory {
            meta: meta_with_requirement(
                "clipboard",
                PluginStatus::Implemented,
                &[types::CLIPBOARD],
                DesktopRequirement::Clipboard,
            ),
            calls,
            panic_on_packet: false,
            hang_on_packet: false,
        })]);

        let empty = Desktop::default();
        assert!(!registry
            .capabilities(&empty)
            .unwrap()
            .accepts(types::CLIPBOARD));

        let mock = MockDesktop::new();
        assert!(registry
            .capabilities(&mock.desktop())
            .unwrap()
            .accepts(types::CLIPBOARD));

        let host = DeviceHost::new(device(), &registry, Desktop::default());
        assert!(!host.is_enabled("clipboard"));

        let host = DeviceHost::new(device(), &registry, MockDesktop::new().desktop());
        assert!(host.is_enabled("clipboard"));
    }

    #[test]
    fn capabilities_require_the_files_adapter() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let registry = Registry::new(vec![Box::new(TestFactory {
            meta: meta_with_requirement(
                "share",
                PluginStatus::Implemented,
                &[types::SHARE_REQUEST],
                DesktopRequirement::Files,
            ),
            calls,
            panic_on_packet: false,
            hang_on_packet: false,
        })]);

        let without_files = MockDesktop::new();
        assert!(!registry
            .capabilities(&without_files.desktop())
            .unwrap()
            .accepts(types::SHARE_REQUEST));
        let host = DeviceHost::new(device(), &registry, without_files.desktop());
        assert!(!host.is_enabled("share"));

        let with_files = MockDesktop::with_files().unwrap();
        assert!(registry
            .capabilities(&with_files.desktop())
            .unwrap()
            .accepts(types::SHARE_REQUEST));
        let host = DeviceHost::new(device(), &registry, with_files.desktop());
        assert!(host.is_enabled("share"));
    }

    #[test]
    fn absorb_keeps_packets_events_and_transfers() {
        let transfer = Transfer::ReceiveFile {
            filename: Some("photo.jpg".to_string()),
            size: PayloadSize::Known(3),
            offer: Some(PayloadOffer {
                size: PayloadSize::Known(3),
                port: 1739,
            }),
        };
        let event = PluginEvent::ShareText {
            text: "hello".to_string(),
        };
        let mut dispatch = Dispatch::default();
        dispatch.absorb(
            PluginOutput::new()
                .with_packet(ping())
                .with_event(event.clone())
                .with_transfer(transfer.clone()),
        );

        assert_eq!(dispatch.packets, vec![ping()]);
        assert_eq!(dispatch.events, vec![event]);
        assert_eq!(dispatch.transfers, vec![transfer]);
    }
}
