//! Pause music during phone calls.

use async_trait::async_trait;
use handfast_desktop::{Media, MediaAction};
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::Value;

use crate::roster;

/// What a telephony packet asks the plugin to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallEvent {
    /// A call is ringing or talking and the peer did not cancel it.
    Active,
    /// The peer cancelled its last ringing or talking event.
    Ended,
    /// A telephony event the plugin does not act on.
    Other,
}

/// Pauses playing media while a call is ringing or talking.
#[derive(Debug, Default)]
pub struct PauseMusicPlugin {
    /// Players this plugin paused, in the order it paused them.
    paused: Vec<String>,
}

impl PauseMusicPlugin {
    /// Create a pause music plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self { paused: Vec::new() }
    }

    async fn pause(&mut self, media: &dyn Media) -> Result<PluginOutput, PluginError> {
        let players = media
            .players()
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;
        let mut paused = 0;
        for player in players {
            if self.paused.contains(&player) {
                continue;
            }
            let playing = media
                .playback(&player)
                .await
                .ok()
                .flatten()
                .is_some_and(|state| state.playing);
            if !playing {
                continue;
            }
            if media.control(&player, MediaAction::Pause).await.is_ok() {
                self.paused.push(player);
                paused += 1;
            }
        }
        Ok(PluginOutput::new().with_event(PluginEvent::Log {
            level: LogLevel::Info,
            message: format!("paused {paused} players for a call"),
        }))
    }

    async fn resume(&mut self, media: &dyn Media) -> Result<PluginOutput, PluginError> {
        let paused = std::mem::take(&mut self.paused);
        let mut resumed = 0;
        for player in &paused {
            if media.control(player, MediaAction::Play).await.is_ok() {
                resumed += 1;
            }
        }
        Ok(PluginOutput::new().with_event(PluginEvent::Log {
            level: LogLevel::Info,
            message: format!("resumed {resumed} players after a call"),
        }))
    }
}

fn is_cancel(body: &serde_json::Map<String, Value>) -> Option<bool> {
    match body.get("isCancel") {
        None => Some(false),
        Some(Value::Bool(cancel)) => Some(*cancel),
        Some(Value::String(cancel)) if cancel == "true" => Some(true),
        Some(Value::String(cancel)) if cancel == "false" => Some(false),
        Some(_) => None,
    }
}

fn parse(packet: &Packet) -> Option<CallEvent> {
    let event = packet.body.get("event")?.as_str()?;
    match event {
        "ringing" | "talking" => match is_cancel(&packet.body)? {
            true => Some(CallEvent::Ended),
            false => Some(CallEvent::Active),
        },
        _ => Some(CallEvent::Other),
    }
}

#[async_trait]
impl Plugin for PauseMusicPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::PAUSEMUSIC
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::TELEPHONY {
            return Ok(PluginOutput::new());
        }
        let Some(event) = parse(packet) else {
            return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: "discarding malformed telephony packet".to_string(),
            }));
        };
        if event == CallEvent::Other {
            return Ok(PluginOutput::new());
        }
        let Some(media) = ctx.desktop.media.as_deref() else {
            return Err(PluginError::UnsupportedDesktop("media"));
        };
        match event {
            CallEvent::Active => self.pause(media).await,
            CallEvent::Ended => self.resume(media).await,
            CallEvent::Other => Ok(PluginOutput::new()),
        }
    }
}

/// Builds pause music plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct PauseMusicFactory;

impl PluginFactory for PauseMusicFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::PAUSEMUSIC
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(PauseMusicPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::{Desktop, MockDesktop, PlaybackState};
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginStatus};
    use serde_json::json;
    use std::sync::Arc;

    /// The mock media backend reports every player as paused, so this wrapper
    /// models a host where some players are playing.
    struct TestMedia {
        inner: Arc<dyn Media>,
        players: Vec<String>,
        playing: Vec<String>,
    }

    #[async_trait]
    impl Media for TestMedia {
        async fn players(&self) -> handfast_desktop::Result<Vec<String>> {
            Ok(self.players.clone())
        }

        async fn playback(&self, player: &str) -> handfast_desktop::Result<Option<PlaybackState>> {
            Ok(Some(PlaybackState {
                player: player.to_string(),
                playing: self.playing.iter().any(|name| name == player),
                title: None,
                artist: None,
                position_ms: 0,
                length_ms: 0,
                volume: None,
            }))
        }

        async fn control(&self, player: &str, action: MediaAction) -> handfast_desktop::Result<()> {
            self.inner.control(player, action).await
        }

        async fn set_volume(&self, player: &str, percent: u8) -> handfast_desktop::Result<()> {
            self.inner.set_volume(player, percent).await
        }
    }

    struct Harness {
        mock: MockDesktop,
        desktop: Desktop,
        device: Device,
        plugin: PauseMusicPlugin,
    }

    impl Harness {
        fn new(players: &[(&str, bool)]) -> Self {
            let mock = MockDesktop::new();
            let mut desktop = mock.desktop();
            let inner = desktop.media.clone().expect("mock media");
            desktop.media = Some(Arc::new(TestMedia {
                inner,
                players: players
                    .iter()
                    .map(|(name, _)| (*name).to_string())
                    .collect(),
                playing: players
                    .iter()
                    .filter(|(_, playing)| *playing)
                    .map(|(name, _)| (*name).to_string())
                    .collect(),
            }));
            Self {
                mock,
                desktop,
                device: Device::new(
                    DeviceId::generate(),
                    DeviceName::new("Test"),
                    DeviceType::Phone,
                ),
                plugin: PauseMusicPlugin::new(),
            }
        }

        async fn run(&mut self, packet: &Packet) -> PluginOutput {
            let mut ctx = PluginContext {
                device: &self.device,
                desktop: &self.desktop,
            };
            self.plugin.on_packet(&mut ctx, packet).await.unwrap()
        }
    }

    fn telephony(body: Value) -> Packet {
        Packet::new(types::TELEPHONY, body).unwrap()
    }

    #[tokio::test]
    async fn ringing_pauses_playing_players() {
        let mut harness = Harness::new(&[("alpha", true), ("beta", true)]);
        let output = harness.run(&telephony(json!({"event": "ringing"}))).await;
        assert_eq!(
            harness.mock.media_actions(),
            vec![
                ("alpha".to_string(), MediaAction::Pause),
                ("beta".to_string(), MediaAction::Pause),
            ]
        );
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Info,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn talking_keeps_players_paused() {
        let mut harness = Harness::new(&[("alpha", true)]);
        harness.run(&telephony(json!({"event": "ringing"}))).await;
        harness.run(&telephony(json!({"event": "talking"}))).await;
        assert_eq!(
            harness.mock.media_actions(),
            vec![("alpha".to_string(), MediaAction::Pause)]
        );
    }

    #[tokio::test]
    async fn call_end_resumes_only_plugin_paused_players() {
        let mut harness = Harness::new(&[("alpha", true), ("beta", true)]);
        harness.run(&telephony(json!({"event": "ringing"}))).await;
        harness.run(&telephony(json!({"event": "talking"}))).await;
        harness
            .run(&telephony(json!({"event": "talking", "isCancel": "true"})))
            .await;
        assert_eq!(
            harness.mock.media_actions(),
            vec![
                ("alpha".to_string(), MediaAction::Pause),
                ("beta".to_string(), MediaAction::Pause),
                ("alpha".to_string(), MediaAction::Play),
                ("beta".to_string(), MediaAction::Play),
            ]
        );
    }

    #[tokio::test]
    async fn user_paused_player_is_not_resumed() {
        let mut harness = Harness::new(&[("playing", true), ("paused", false)]);
        harness.run(&telephony(json!({"event": "ringing"}))).await;
        harness
            .run(&telephony(json!({"event": "ringing", "isCancel": true})))
            .await;
        assert_eq!(
            harness.mock.media_actions(),
            vec![
                ("playing".to_string(), MediaAction::Pause),
                ("playing".to_string(), MediaAction::Play),
            ]
        );
    }

    #[tokio::test]
    async fn malformed_telephony_is_a_warning() {
        let mut harness = Harness::new(&[("alpha", true)]);
        let output = harness.run(&Packet::empty(types::TELEPHONY).unwrap()).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
        let output = harness.run(&telephony(json!({"event": 1}))).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
        assert!(harness.mock.media_actions().is_empty());
    }

    #[test]
    fn meta_matches_roster() {
        assert_eq!(PauseMusicPlugin::new().meta(), &roster::PAUSEMUSIC);
        assert_eq!(PauseMusicFactory.meta(), &roster::PAUSEMUSIC);
        assert_eq!(roster::PAUSEMUSIC.status, PluginStatus::Implemented);
        assert!(roster::PAUSEMUSIC.outgoing.is_empty());
    }
}
