//! Give the phone an extra desktop output.
//!
//! The protocol is symmetric and small. The phone sends
//! `kdeconnect.virtualmonitor.request {"start": true, "width": ..., "height": ...}`
//! and the desktop creates a virtual display; `{"start": false}` removes it.
//! The desktop's reply on `kdeconnect.virtualmonitor` echoes whether the display
//! is now enabled and its geometry, which is what lets the phone draw the
//! extra screen in its viewer.
//!
//! The rule worth stating: an unsupported request is answered with
//! `enabled: false`, never with silence and never with an error packet. The
//! phone's UI has no error path — a start that produces nothing would leave it
//! waiting forever.

use async_trait::async_trait;
use handfast_desktop::VirtualDisplay;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    DesktopRequirement, LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory,
    PluginMeta, PluginOutput,
};
use serde_json::{json, Map, Value};

use crate::roster;

/// Largest dimension accepted from a peer, matching the desktop backend's cap.
const MAX_DIMENSION: u32 = 8192;

/// The geometry the phone asked for, already validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestedDisplay {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Scale percentage, 100 when the phone did not say.
    pub scale: u16,
}

impl RequestedDisplay {
    /// Read a request body, or `None` when it is unusable.
    ///
    /// A missing dimension is refused rather than defaulted: guessing a size
    /// produces a display the phone did not ask for, which is worse than a
    /// clean failure.
    #[must_use]
    pub fn from_body(body: &Value) -> Option<Self> {
        let width = body.get("width")?.as_u64()?;
        let height = body.get("height")?.as_u64()?;
        let too_large =
            width > u64::from(MAX_DIMENSION) || height > u64::from(MAX_DIMENSION);
        if width == 0 || height == 0 || too_large {
            return None;
        }
        let scale = body
            .get("scale")
            .and_then(Value::as_u64)
            .filter(|scale| (50..=400).contains(scale))
            .unwrap_or(100);
        Some(Self {
            width: u32::try_from(width).ok()?,
            height: u32::try_from(height).ok()?,
            scale: u16::try_from(scale).ok()?,
        })
    }

    fn to_desktop(self) -> VirtualDisplay {
        VirtualDisplay {
            width: self.width,
            height: self.height,
            scale: self.scale,
        }
    }
}

/// Creates and removes a virtual display for the peer.
#[derive(Debug, Default)]
pub struct VirtualMonitorPlugin {
    /// Current requested geometry, when a display should exist.
    current: Option<RequestedDisplay>,
}

impl VirtualMonitorPlugin {
    /// Create a virtual monitor plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self { current: None }
    }

    /// The geometry currently requested, when one is active.
    #[must_use]
    pub const fn current(&self) -> Option<RequestedDisplay> {
        self.current
    }

    /// Reply describing the display state.
    fn reply(enabled: bool, display: Option<RequestedDisplay>) -> Option<Packet> {
        let mut body = Map::new();
        body.insert("enabled".to_string(), json!(enabled));
        if let Some(display) = display {
            body.insert("width".to_string(), json!(display.width));
            body.insert("height".to_string(), json!(display.height));
            body.insert("scale".to_string(), json!(display.scale));
        }
        Packet::new(types::VIRTUALMONITOR, Value::Object(body)).ok()
    }
}

#[async_trait]
impl Plugin for VirtualMonitorPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::VIRTUALMONITOR
    }

    fn requirements(&self) -> &'static [DesktopRequirement] {
        &[DesktopRequirement::Display]
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::VIRTUALMONITOR_REQUEST {
            return Ok(PluginOutput::new());
        }

        let start = packet
            .body
            .get("start")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let Some(display) = &ctx.desktop.display else {
            self.current = None;
            return Ok(PluginOutput::new()
                .with_optional_packet(VirtualMonitorPlugin::reply(false, None))
                .with_event(PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: "peer asked for a virtual display but the host has no \
                              compositor backend"
                        .to_string(),
                }));
        };

        if !start {
            let was_active = self.current.take().is_some();
            let result = display.disable().await;
            let mut output = PluginOutput::new();
            if let Err(error) = result {
                output = output.with_event(PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: format!("could not remove the virtual display: {error}"),
                });
            }
            return Ok(output
                .with_optional_packet(VirtualMonitorPlugin::reply(false, None))
                .with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message: if was_active {
                        "virtual display removed at the peer's request".to_string()
                    } else {
                        "peer asked to remove a display that was not active".to_string()
                    },
                }));
        }

        let Some(requested) = RequestedDisplay::from_body(&packet.body) else {
            return Ok(PluginOutput::new()
                .with_optional_packet(VirtualMonitorPlugin::reply(false, None))
                .with_event(PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: "peer asked for a virtual display with an unusable geometry"
                        .to_string(),
                }));
        };

        match display.enable(requested.to_desktop()).await {
            Ok(()) => {
                self.current = Some(requested);
                Ok(PluginOutput::new()
                    .with_optional_packet(VirtualMonitorPlugin::reply(true, Some(requested)))
                    .with_event(PluginEvent::Log {
                        level: LogLevel::Info,
                        message: format!(
                            "virtual display enabled at {}x{} @{}%",
                            requested.width, requested.height, requested.scale
                        ),
                    }))
            }
            Err(error) => {
                self.current = None;
                Ok(PluginOutput::new()
                    .with_optional_packet(VirtualMonitorPlugin::reply(false, None))
                    .with_event(PluginEvent::Log {
                        level: LogLevel::Warn,
                        message: format!("virtual display failed: {error}"),
                    }))
            }
        }
    }
}

/// Builds [`VirtualMonitorPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct VirtualMonitorFactory;

impl PluginFactory for VirtualMonitorFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::VIRTUALMONITOR
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(VirtualMonitorPlugin::new())
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

    async fn send(
        plugin: &mut VirtualMonitorPlugin,
        desktop: &handfast_desktop::Desktop,
        body: Value,
    ) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
            config: PluginConfig::empty(),
        };
        let packet = Packet::from_body(types::VIRTUALMONITOR_REQUEST, &body).unwrap();
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    #[test]
    fn the_roster_entry_is_symmetric() {
        assert_eq!(roster::VIRTUALMONITOR.status, PluginStatus::Implemented);
        assert_eq!(
            roster::VIRTUALMONITOR.incoming,
            &[types::VIRTUALMONITOR_REQUEST]
        );
        assert_eq!(roster::VIRTUALMONITOR.outgoing, &[types::VIRTUALMONITOR]);
    }

    #[tokio::test]
    async fn a_start_request_creates_the_display_and_reports_it() {
        let mock = MockDesktop::new();
        let mut plugin = VirtualMonitorPlugin::new();
        let output = send(
            &mut plugin,
            &mock.desktop(),
            json!({ "start": true, "width": 1280, "height": 720 }),
        )
        .await;

        assert_eq!(output.packets.len(), 1);
        let body = &output.packets[0].body;
        assert_eq!(output.packets[0].ptype, types::VIRTUALMONITOR);
        assert_eq!(body.get("enabled"), Some(&json!(true)));
        assert_eq!(body.get("width"), Some(&json!(1280)));
        assert_eq!(body.get("height"), Some(&json!(720)));
        // Scale defaults to 1.0 when the phone omits it.
        assert_eq!(body.get("scale"), Some(&json!(100)));

        assert_eq!(
            mock.virtual_display(),
            Some(VirtualDisplay {
                width: 1280,
                height: 720,
                scale: 100
            })
        );
    }

    #[tokio::test]
    async fn a_stop_request_removes_the_display() {
        let mock = MockDesktop::new();
        let mut plugin = VirtualMonitorPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            json!({ "start": true, "width": 800, "height": 600 }),
        )
        .await;

        let output = send(&mut plugin, &mock.desktop(), json!({ "start": false })).await;
        assert_eq!(output.packets[0].body.get("enabled"), Some(&json!(false)));
        assert!(mock.virtual_display().is_none());
        assert!(plugin.current().is_none());
    }

    #[tokio::test]
    async fn an_unusable_geometry_is_refused_with_a_reply() {
        let mock = MockDesktop::new();
        let mut plugin = VirtualMonitorPlugin::new();

        for body in [
            json!({ "start": true }),
            json!({ "start": true, "width": 0, "height": 720 }),
            json!({ "start": true, "width": 99999, "height": 720 }),
        ] {
            let output = send(&mut plugin, &mock.desktop(), body.clone()).await;
            assert_eq!(
                output.packets[0].body.get("enabled"),
                Some(&json!(false)),
                "body {body} must be answered, not dropped"
            );
            assert!(mock.virtual_display().is_none());
        }
    }

    #[tokio::test]
    async fn a_host_without_a_display_backend_still_answers() {
        let desktop = handfast_desktop::Desktop::default();
        let mut plugin = VirtualMonitorPlugin::new();
        let output = send(
            &mut plugin,
            &desktop,
            json!({ "start": true, "width": 1280, "height": 720 }),
        )
        .await;

        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].body.get("enabled"), Some(&json!(false)));
        assert_eq!(output.events.len(), 1);
    }

    #[test]
    fn scale_is_validated_but_optional() {
        assert_eq!(
            RequestedDisplay::from_body(&json!({ "width": 100, "height": 100, "scale": 200 }))
                .unwrap()
                .scale,
            200
        );
        // Out-of-range scale falls back rather than refusing the request.
        assert_eq!(
            RequestedDisplay::from_body(&json!({ "width": 100, "height": 100, "scale": 900 }))
                .unwrap()
                .scale,
            100
        );
        assert!(RequestedDisplay::from_body(&json!({ "width": 100 })).is_none());
    }
}
