//! Lock device plugin.
//!
//! The protocol carries lock state in both directions. A peer asks with
//! `kdeconnect.lock.request {"setLocked": true}`; the desktop answers
//! `kdeconnect.lock {"lockResult": bool}` and reports the resulting state with
//! `{"isLocked": bool}`. On connect the desktop client sends
//! `{"requestLocked": true}` so the remote can show the current state.
//!
//! Lock only: Handfast never exposes an unlock, so a remote peer cannot take
//! over an unattended session. A `setLocked: false` request is acknowledged as
//! refused and does not report the desktop as unlocked.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Value};

use crate::roster;

/// Locks the desktop when a paired device asks, and reports its state.
#[derive(Debug, Default)]
pub struct LockDevicePlugin {
    /// Whether this desktop is locked as far as Handfast knows.
    locked: bool,
}

impl LockDevicePlugin {
    /// Create a lock device plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self { locked: false }
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

fn lock_packet(body: Value) -> Result<Packet, PluginError> {
    Packet::new(types::LOCK, body).map_err(|error| PluginError::InvalidPacket(error.to_string()))
}

fn state_packet(locked: bool) -> Result<Packet, PluginError> {
    lock_packet(json!({ "isLocked": locked }))
}

fn result_packet(success: bool) -> Result<Packet, PluginError> {
    lock_packet(json!({ "lockResult": success }))
}

#[async_trait]
impl Plugin for LockDevicePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::LOCKDEVICE
    }

    async fn on_connect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        if ctx.desktop.lock.is_none() {
            return Ok(warn("lock request without a lock adapter"));
        }
        let request = Packet::new(types::LOCK_REQUEST, json!({ "requestLocked": true }))
            .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
        Ok(PluginOutput::new().with_packet(request))
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::LOCK_REQUEST {
            return Ok(PluginOutput::new());
        }

        // A bare `requestLocked` asks for the current state.
        if packet
            .body
            .get("requestLocked")
            .is_some_and(|value| value.is_boolean() || matches!(value, Value::Null))
        {
            return Ok(PluginOutput::new().with_packet(state_packet(self.locked)?));
        }

        let locked = match requested_lock(packet) {
            Ok(locked) => locked,
            Err(()) => return Ok(warn("lock request with a malformed setLocked")),
        };
        if !locked {
            // Report the refusal either way, so the peer's UI does not show a
            // successful unlock.
            let mut output = PluginOutput::new().with_packet(result_packet(false)?);
            output.packets.push(state_packet(self.locked)?);
            output.events.push(PluginEvent::Log {
                level: LogLevel::Warn,
                message: "refusing to unlock because remote unlock is not supported".to_string(),
            });
            return Ok(output);
        }

        let Some(lock) = ctx.desktop.lock.as_ref() else {
            let mut output = PluginOutput::new().with_packet(result_packet(false)?);
            output.events.push(PluginEvent::Log {
                level: LogLevel::Warn,
                message: "lock request without a lock adapter".to_string(),
            });
            return Ok(output);
        };
        let result = lock.lock().await;
        let success = result.is_ok();
        if let Err(error) = result {
            let mut output = PluginOutput::new().with_packet(result_packet(false)?);
            output.events.push(PluginEvent::Log {
                level: LogLevel::Warn,
                message: format!("lock failed: {error}"),
            });
            return Ok(output);
        }
        self.locked = true;

        Ok(PluginOutput::new()
            .with_packet(result_packet(success)?)
            .with_packet(state_packet(self.locked)?)
            .with_event(PluginEvent::Log {
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

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn run_with(
        plugin: &mut LockDevicePlugin,
        desktop: &Desktop,
        body: Value,
    ) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        let packet = Packet::from_body(types::LOCK_REQUEST, &body).unwrap();
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    async fn run(desktop: &Desktop, body: Value) -> PluginOutput {
        run_with(&mut LockDevicePlugin::new(), desktop, body).await
    }

    #[tokio::test]
    async fn a_lock_request_locks_and_reports() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), json!({"setLocked": true})).await;
        assert_eq!(mock.lock_calls(), 1);
        assert_eq!(output.packets.len(), 2);
        assert_eq!(output.packets[0].ptype, types::LOCK);
        assert_eq!(output.packets[0].body["lockResult"], json!(true));
        assert_eq!(output.packets[1].body["isLocked"], json!(true));
    }

    #[tokio::test]
    async fn an_unlock_request_is_refused_but_acknowledged() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), json!({"setLocked": false})).await;
        assert_eq!(mock.lock_calls(), 0);
        assert_eq!(output.packets[0].body["lockResult"], json!(false));
        assert_eq!(output.packets[1].body["isLocked"], json!(false));
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn request_locked_answers_with_the_state() {
        let mock = MockDesktop::new();
        let mut plugin = LockDevicePlugin::new();
        run_with(&mut plugin, &mock.desktop(), json!({"setLocked": true})).await;
        let output = run_with(&mut plugin, &mock.desktop(), json!({"requestLocked": null})).await;
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].body["isLocked"], json!(true));
    }

    #[tokio::test]
    async fn connect_asks_for_the_remote_state() {
        let mock = MockDesktop::new();
        let device = device();
        let desktop = mock.desktop();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let output = LockDevicePlugin::new().on_connect(&mut ctx).await.unwrap();
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::LOCK_REQUEST);
        assert!(output.packets[0].body.contains_key("requestLocked"));
    }

    #[tokio::test]
    async fn a_malformed_request_is_rejected_without_locking() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), json!({"setLocked": 1})).await;
        assert_eq!(mock.lock_calls(), 0);
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn a_host_without_a_lock_adapter_reports_failure() {
        let output = run(&Desktop::default(), json!({"setLocked": true})).await;
        assert_eq!(output.packets[0].body["lockResult"], json!(false));
    }
}
