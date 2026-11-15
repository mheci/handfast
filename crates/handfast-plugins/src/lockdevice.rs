//! Lock device plugin.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::Value;

use crate::roster;

/// Locks the desktop when a paired device asks.
#[derive(Debug, Default)]
pub struct LockDevicePlugin;

impl LockDevicePlugin {
    /// Create a lock device plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

fn warn(message: &str) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level: LogLevel::Warn,
        message: message.to_string(),
    })
}

fn requested_lock(packet: &Packet) -> Result<bool, ()> {
    match packet.body.get("setLocked") {
        Some(Value::Bool(locked)) => Ok(*locked),
        Some(Value::String(text)) if text == "true" => Ok(true),
        Some(Value::String(text)) if text == "false" => Ok(false),
        _ => Err(()),
    }
}

#[async_trait]
impl Plugin for LockDevicePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::LOCKDEVICE
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::LOCK_REQUEST {
            return Ok(PluginOutput::new());
        }

        let locked = match requested_lock(packet) {
            Ok(locked) => locked,
            Err(()) => return Ok(warn("lock request with a malformed setLocked")),
        };
        if !locked {
            return Ok(warn(
                "refusing to unlock because remote unlock is not supported",
            ));
        }

        let Some(lock) = ctx.desktop.lock.as_ref() else {
            return Ok(warn("lock request without a lock adapter"));
        };
        lock.lock()
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;

        Ok(PluginOutput::new().with_event(PluginEvent::Log {
            level: LogLevel::Info,
            message: format!("locked the desktop for {}", ctx.device.name),
        }))
    }
}

/// Builds lock device plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct LockDeviceFactory;

impl PluginFactory for LockDeviceFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::LOCKDEVICE
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(LockDevicePlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::{Desktop, MockDesktop};
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

    async fn run(desktop: &Desktop, body: serde_json::Value) -> PluginOutput {
        let packet = Packet::from_body(types::LOCK_REQUEST, &body).unwrap();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        LockDevicePlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap()
    }

    fn is_warning(output: &PluginOutput) -> bool {
        matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        )
    }

    #[tokio::test]
    async fn lock_request_locks_the_desktop() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), json!({"setLocked": true})).await;

        assert_eq!(mock.lock_calls(), 1);
        assert_eq!(
            output.events,
            vec![PluginEvent::Log {
                level: LogLevel::Info,
                message: "locked the desktop for Test".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn string_true_locks_the_desktop() {
        let mock = MockDesktop::new();
        run(&mock.desktop(), json!({"setLocked": "true"})).await;

        assert_eq!(mock.lock_calls(), 1);
    }

    #[tokio::test]
    async fn unlock_request_is_refused() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), json!({"setLocked": false})).await;

        assert_eq!(mock.lock_calls(), 0);
        assert!(is_warning(&output));
    }

    #[tokio::test]
    async fn string_false_is_refused() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), json!({"setLocked": "false"})).await;

        assert_eq!(mock.lock_calls(), 0);
        assert!(is_warning(&output));
    }

    #[tokio::test]
    async fn malformed_set_locked_is_a_warning() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), json!({"setLocked": 1})).await;

        assert_eq!(mock.lock_calls(), 0);
        assert!(is_warning(&output));
    }

    #[tokio::test]
    async fn missing_set_locked_is_a_warning() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), json!({})).await;

        assert_eq!(mock.lock_calls(), 0);
        assert!(is_warning(&output));
    }

    #[tokio::test]
    async fn missing_adapter_is_a_warning() {
        let output = run(&Desktop::default(), json!({"setLocked": true})).await;

        assert!(is_warning(&output));
    }

    #[test]
    fn lock_request_is_advertised_only_with_a_lock_adapter() {
        let registry = crate::registry();

        let without = registry.capabilities(&Desktop::default()).unwrap();
        assert!(!without.accepts(types::LOCK_REQUEST));

        let with = registry
            .capabilities(&MockDesktop::new().desktop())
            .unwrap();
        assert!(with.accepts(types::LOCK_REQUEST));
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(LockDevicePlugin::new().meta(), &roster::LOCKDEVICE);
        assert_eq!(LockDeviceFactory.meta(), &roster::LOCKDEVICE);
        assert_eq!(
            roster::LOCKDEVICE.status,
            handfast_runtime::PluginStatus::Implemented
        );
    }
}
