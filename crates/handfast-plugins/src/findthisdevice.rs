//! Find this device plugin.

use async_trait::async_trait;
use handfast_desktop::Notification;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::Value;

use crate::roster;

const TITLE: &str = "Find this device";

/// Notifies the desktop when a paired device asks to be found.
#[derive(Debug, Default)]
pub struct FindThisDevicePlugin;

impl FindThisDevicePlugin {
    /// Create a find this device plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

fn message(packet: &Packet) -> Result<Option<String>, ()> {
    match packet.body.get("message") {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(()),
    }
}

#[async_trait]
impl Plugin for FindThisDevicePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::FINDTHISDEVICE
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if !matches!(
            packet.ptype.as_str(),
            types::FINDMYPHONE_REQUEST | types::FINDTHISDEVICE_PRIVATE
        ) {
            return Ok(PluginOutput::new());
        }

        let message = match message(packet) {
            Ok(None) => format!("{} is asking you to find this device", ctx.device.name),
            Ok(Some(message)) => message,
            Err(()) => {
                return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: "find this device request with a malformed message".to_string(),
                }));
            }
        };

        let Some(notifier) = ctx.desktop.notifier.as_ref() else {
            return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: "find this device request without a notifications adapter".to_string(),
            }));
        };
        notifier
            .notify(&Notification::new("Handfast", TITLE, message))
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;

        Ok(PluginOutput::new().with_event(PluginEvent::Log {
            level: LogLevel::Info,
            message: format!("find this device request from {}", ctx.device.name),
        }))
    }
}

/// Builds find this device plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct FindThisDeviceFactory;

impl PluginFactory for FindThisDeviceFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::FINDTHISDEVICE
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(FindThisDevicePlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;
    use serde_json::json;

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn run(desktop: &handfast_desktop::Desktop, packet: &Packet) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        FindThisDevicePlugin::new()
            .on_packet(&mut ctx, packet)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn request_notifies() {
        let mock = MockDesktop::new();
        let packet = Packet::empty(types::FINDMYPHONE_REQUEST).unwrap();
        let output = run(&mock.desktop(), &packet).await;

        assert_eq!(
            mock.notifications(),
            vec![Notification::new(
                "Handfast",
                "Find this device",
                "Test is asking you to find this device"
            )]
        );
        assert_eq!(
            output.events,
            vec![PluginEvent::Log {
                level: LogLevel::Info,
                message: "find this device request from Test".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn request_includes_optional_message() {
        let mock = MockDesktop::new();
        let packet = Packet::from_body(
            types::FINDMYPHONE_REQUEST,
            &json!({"message": "Ring the office"}),
        )
        .unwrap();
        run(&mock.desktop(), &packet).await;

        assert_eq!(mock.notifications()[0].text, "Ring the office");
    }

    #[tokio::test]
    async fn malformed_request_is_a_warning() {
        let mock = MockDesktop::new();
        let packet =
            Packet::from_body(types::FINDMYPHONE_REQUEST, &json!({"message": 42})).unwrap();
        let output = run(&mock.desktop(), &packet).await;

        assert!(mock.notifications().is_empty());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn missing_notifier_is_a_warning() {
        let desktop = handfast_desktop::Desktop::default();
        let packet = Packet::empty(types::FINDMYPHONE_REQUEST).unwrap();
        let output = run(&desktop, &packet).await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[test]
    fn meta_matches_roster() {
        assert_eq!(&roster::FINDTHISDEVICE, FindThisDevicePlugin::new().meta());
        assert_eq!(&roster::FINDTHISDEVICE, FindThisDeviceFactory.meta());
        assert_eq!(
            roster::FINDTHISDEVICE.status,
            handfast_runtime::PluginStatus::Implemented
        );
    }
}
