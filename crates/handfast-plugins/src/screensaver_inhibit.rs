//! Screensaver inhibit plugin.

use async_trait::async_trait;
use handfast_proto::Packet;
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};

use crate::roster;

const REASON: &str = "Phone is connected";

/// Inhibits the screensaver while a device is connected.
#[derive(Debug, Default)]
pub struct ScreensaverInhibitPlugin {
    inhibited: bool,
}

impl ScreensaverInhibitPlugin {
    /// Create a screensaver inhibit plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self { inhibited: false }
    }
}

fn warn(message: &str) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level: LogLevel::Warn,
        message: message.to_string(),
    })
}

#[async_trait]
impl Plugin for ScreensaverInhibitPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SCREENSAVER_INHIBIT
    }

    async fn on_connect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        let Some(screensaver) = ctx.desktop.screensaver.as_ref() else {
            return Ok(warn("screensaver adapter unavailable"));
        };
        if self.inhibited {
            return Ok(PluginOutput::new());
        }
        screensaver
            .inhibit(REASON)
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;
        self.inhibited = true;
        Ok(PluginOutput::new())
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        _packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        Ok(PluginOutput::new())
    }

    async fn on_disconnect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        let Some(screensaver) = ctx.desktop.screensaver.as_ref() else {
            return Ok(warn("screensaver adapter unavailable"));
        };
        if !self.inhibited {
            return Ok(PluginOutput::new());
        }
        screensaver
            .release()
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;
        self.inhibited = false;
        Ok(PluginOutput::new())
    }
}

/// Builds screensaver inhibit plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScreensaverInhibitFactory;

impl PluginFactory for ScreensaverInhibitFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SCREENSAVER_INHIBIT
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(ScreensaverInhibitPlugin::new())
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

    async fn connect(desktop: &Desktop, plugin: &mut ScreensaverInhibitPlugin) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        plugin.on_connect(&mut ctx).await.unwrap()
    }

    async fn disconnect(desktop: &Desktop, plugin: &mut ScreensaverInhibitPlugin) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        plugin.on_disconnect(&mut ctx).await.unwrap()
    }

    #[tokio::test]
    async fn connect_inhibits_the_screensaver() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = ScreensaverInhibitPlugin::new();
        let output = connect(&desktop, &mut plugin).await;
        assert!(mock.inhibited());
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn disconnect_releases_the_screensaver() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = ScreensaverInhibitPlugin::new();
        connect(&desktop, &mut plugin).await;
        let output = disconnect(&desktop, &mut plugin).await;
        assert!(!mock.inhibited());
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn missing_adapter_warns_on_connect() {
        let desktop = Desktop::default();
        let mut plugin = ScreensaverInhibitPlugin::new();
        let output = connect(&desktop, &mut plugin).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn missing_adapter_warns_on_disconnect() {
        let desktop = Desktop::default();
        let mut plugin = ScreensaverInhibitPlugin::new();
        let output = disconnect(&desktop, &mut plugin).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(
            ScreensaverInhibitPlugin::new().meta(),
            &roster::SCREENSAVER_INHIBIT
        );
        assert_eq!(
            ScreensaverInhibitFactory.meta(),
            &roster::SCREENSAVER_INHIBIT
        );
    }
}
