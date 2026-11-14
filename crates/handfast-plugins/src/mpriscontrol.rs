//! MPRIS control plugin.

use async_trait::async_trait;
use handfast_desktop::{Media, MediaAction};
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::Value;

use crate::roster;

const ACTIONS: &[(&str, MediaAction)] = &[
    ("Play", MediaAction::Play),
    ("Pause", MediaAction::Pause),
    ("PlayPause", MediaAction::PlayPause),
    ("Stop", MediaAction::Stop),
    ("Next", MediaAction::Next),
    ("Previous", MediaAction::Previous),
];

/// Upper bound for a peer-supplied player name before it becomes a `playerctl` argument.
const MAX_PLAYER_BYTES: usize = 256;

struct Request<'a> {
    player: Option<&'a str>,
    action: Option<(&'static str, MediaAction)>,
    volume: Option<u8>,
}

fn parse(packet: &Packet) -> Result<Request<'_>, String> {
    let player = match packet.body.get("player") {
        None => None,
        Some(Value::String(name)) if name.is_empty() => {
            return Err("player is empty".to_string());
        }
        Some(Value::String(name)) if name.len() > MAX_PLAYER_BYTES => {
            return Err(format!("player is longer than {MAX_PLAYER_BYTES} bytes"));
        }
        Some(Value::String(name)) if name.chars().any(char::is_control) => {
            return Err("player contains control characters".to_string());
        }
        Some(Value::String(name)) => Some(name.as_str()),
        Some(_) => return Err("player is not a string".to_string()),
    };
    let action = match packet.body.get("action") {
        None => None,
        Some(Value::String(name)) => Some(
            ACTIONS
                .iter()
                .find(|(wire, _)| *wire == name.as_str())
                .copied()
                .ok_or_else(|| format!("unsupported action {name:?}"))?,
        ),
        Some(_) => return Err("action is not a string".to_string()),
    };
    let volume = match packet.body.get("setVolume") {
        None => None,
        Some(value) => {
            let value = value
                .as_i64()
                .ok_or_else(|| "setVolume is not an integer".to_string())?;
            Some(value.clamp(0, 100) as u8)
        }
    };
    if action.is_none() && volume.is_none() {
        return Err("no supported action".to_string());
    }
    Ok(Request {
        player,
        action,
        volume,
    })
}

fn describe(request: &Request<'_>) -> String {
    let mut parts = Vec::new();
    if let Some((name, _)) = request.action {
        parts.push((*name).to_string());
    }
    if let Some(volume) = request.volume {
        parts.push(format!("volume {volume}"));
    }
    parts.join(" and ")
}

async fn apply(media: &dyn Media, player: &str, request: &Request<'_>) -> Result<(), String> {
    if let Some((_, action)) = request.action {
        media
            .control(player, action)
            .await
            .map_err(|error| error.to_string())?;
    }
    if let Some(volume) = request.volume {
        media
            .set_volume(player, volume)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Applies MPRIS transport and volume requests to desktop players.
#[derive(Debug, Default)]
pub struct MprisControlPlugin;

impl MprisControlPlugin {
    /// Create an MPRIS control plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Plugin for MprisControlPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::MPRISCONTROL
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::MPRIS_REQUEST {
            return Ok(PluginOutput::new());
        }
        let request = match parse(packet) {
            Ok(request) => request,
            Err(reason) => {
                return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: format!("mpris request ignored: {reason}"),
                }));
            }
        };
        let Some(media) = ctx.desktop.media.as_ref() else {
            return Err(PluginError::UnsupportedDesktop("media"));
        };
        let targets = match request.player {
            Some(player) => vec![player.to_string()],
            None => media
                .players()
                .await
                .map_err(|error| PluginError::Backend(error.to_string()))?,
        };
        let operation = describe(&request);
        if targets.is_empty() {
            return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Info,
                message: format!("mpris {operation} skipped: no players"),
            }));
        }

        let mut applied = 0usize;
        let mut failures = Vec::new();
        for target in &targets {
            match apply(media.as_ref(), target, &request).await {
                Ok(()) => applied += 1,
                Err(message) => failures.push(format!("{target}: {message}")),
            }
        }
        let event = if failures.is_empty() {
            let target = match request.player {
                Some(player) => player.to_string(),
                None => format!("{} players", targets.len()),
            };
            PluginEvent::Log {
                level: LogLevel::Info,
                message: format!("mpris {operation} applied to {target}"),
            }
        } else {
            PluginEvent::Log {
                level: LogLevel::Warn,
                message: format!(
                    "mpris {operation} failed for {}; applied to {applied}",
                    failures.join(", ")
                ),
            }
        };
        Ok(PluginOutput::new().with_event(event))
    }
}

/// Builds MPRIS control plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct MprisControlFactory;

impl PluginFactory for MprisControlFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::MPRISCONTROL
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(MprisControlPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use handfast_desktop::{Desktop, MockDesktop, PlaybackState, Result as DesktopResult};
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginStatus};
    use serde_json::json;

    struct RecordingMedia {
        players: Vec<String>,
        actions: Mutex<Vec<(String, MediaAction)>>,
        volumes: Mutex<Vec<(String, u8)>>,
        listed: Mutex<usize>,
    }

    impl RecordingMedia {
        fn new(players: &[&str]) -> Self {
            Self {
                players: players.iter().map(|player| (*player).to_string()).collect(),
                actions: Mutex::new(Vec::new()),
                volumes: Mutex::new(Vec::new()),
                listed: Mutex::new(0),
            }
        }

        fn actions(&self) -> Vec<(String, MediaAction)> {
            self.actions.lock().unwrap().clone()
        }

        fn volumes(&self) -> Vec<(String, u8)> {
            self.volumes.lock().unwrap().clone()
        }

        fn listed(&self) -> usize {
            *self.listed.lock().unwrap()
        }
    }

    #[async_trait]
    impl Media for RecordingMedia {
        async fn players(&self) -> DesktopResult<Vec<String>> {
            *self.listed.lock().unwrap() += 1;
            Ok(self.players.clone())
        }

        async fn playback(&self, _player: &str) -> DesktopResult<Option<PlaybackState>> {
            Ok(None)
        }

        async fn control(&self, player: &str, action: MediaAction) -> DesktopResult<()> {
            self.actions
                .lock()
                .unwrap()
                .push((player.to_string(), action));
            Ok(())
        }

        async fn set_volume(&self, player: &str, percent: u8) -> DesktopResult<()> {
            self.volumes
                .lock()
                .unwrap()
                .push((player.to_string(), percent));
            Ok(())
        }
    }

    fn recording_desktop(players: &[&str]) -> (Arc<RecordingMedia>, Desktop) {
        let media = Arc::new(RecordingMedia::new(players));
        let mut desktop = MockDesktop::new().desktop();
        desktop.media = Some(Arc::clone(&media) as Arc<dyn Media>);
        (media, desktop)
    }

    fn request(body: serde_json::Value) -> Packet {
        Packet::from_body(types::MPRIS_REQUEST, &body).unwrap()
    }

    async fn run_with(desktop: &Desktop, packet: &Packet) -> Result<PluginOutput, PluginError> {
        let device = Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        );
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        MprisControlPlugin::new().on_packet(&mut ctx, packet).await
    }

    fn assert_log(output: &PluginOutput, level: LogLevel) {
        assert!(
            matches!(
                output.events.as_slice(),
                [PluginEvent::Log {
                    level: event_level,
                    ..
                }] if *event_level == level
            ),
            "expected one {level:?} log, got {:?}",
            output.events
        );
    }

    #[tokio::test]
    async fn every_supported_action_reaches_the_media_adapter() {
        let cases = [
            ("Play", MediaAction::Play),
            ("Pause", MediaAction::Pause),
            ("PlayPause", MediaAction::PlayPause),
            ("Stop", MediaAction::Stop),
            ("Next", MediaAction::Next),
            ("Previous", MediaAction::Previous),
        ];
        for (wire, expected) in cases {
            let mock = MockDesktop::new();
            let desktop = mock.desktop();
            let output = run_with(
                &desktop,
                &request(json!({"player": "Mock Player", "action": wire})),
            )
            .await
            .unwrap();
            assert_eq!(
                mock.media_actions(),
                vec![("Mock Player".to_string(), expected)],
                "action {wire}"
            );
            assert_log(&output, LogLevel::Info);
        }
    }

    #[tokio::test]
    async fn named_player_is_targeted() {
        let (media, desktop) = recording_desktop(&["Living Room", "Kitchen"]);
        let output = run_with(
            &desktop,
            &request(json!({"player": "Kitchen", "action": "Play"})),
        )
        .await
        .unwrap();
        assert_eq!(
            media.actions(),
            vec![("Kitchen".to_string(), MediaAction::Play)]
        );
        assert_log(&output, LogLevel::Info);
    }

    #[tokio::test]
    async fn unnamed_packet_reaches_every_player() {
        let (media, desktop) = recording_desktop(&["Living Room", "Kitchen"]);
        run_with(&desktop, &request(json!({"action": "Next"})))
            .await
            .unwrap();
        assert_eq!(
            media.actions(),
            vec![
                ("Living Room".to_string(), MediaAction::Next),
                ("Kitchen".to_string(), MediaAction::Next),
            ]
        );
    }

    #[tokio::test]
    async fn unsupported_action_is_a_warning() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let output = run_with(
            &desktop,
            &request(json!({"player": "Mock Player", "action": "FastForward"})),
        )
        .await
        .unwrap();
        assert_log(&output, LogLevel::Warn);
        assert!(mock.media_actions().is_empty());
    }

    #[tokio::test]
    async fn oversized_player_is_a_warning() {
        let (media, desktop) = recording_desktop(&["Kitchen"]);
        let player = "p".repeat(MAX_PLAYER_BYTES + 1);
        let output = run_with(
            &desktop,
            &request(json!({"player": player, "action": "Play"})),
        )
        .await
        .unwrap();

        assert_log(&output, LogLevel::Warn);
        assert!(media.actions().is_empty());
        assert!(media.volumes().is_empty());
        assert_eq!(media.listed(), 0);
    }

    #[tokio::test]
    async fn control_character_in_player_is_a_warning() {
        let (media, desktop) = recording_desktop(&["Kitchen"]);
        let output = run_with(
            &desktop,
            &request(json!({"player": "Kit\u{0}chen", "action": "Play"})),
        )
        .await
        .unwrap();

        assert_log(&output, LogLevel::Warn);
        assert!(media.actions().is_empty());
        assert!(media.volumes().is_empty());
        assert_eq!(media.listed(), 0);
    }

    #[tokio::test]
    async fn unknown_action_does_not_call_media() {
        let (media, desktop) = recording_desktop(&["Kitchen"]);
        let output = run_with(&desktop, &request(json!({"action": "FastForward"})))
            .await
            .unwrap();

        assert_log(&output, LogLevel::Warn);
        assert!(media.actions().is_empty());
        assert!(media.volumes().is_empty());
        assert_eq!(media.listed(), 0);
    }

    #[tokio::test]
    async fn malformed_request_is_a_warning() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let output = run_with(&desktop, &request(json!({"setVolume": "loud"})))
            .await
            .unwrap();
        assert_log(&output, LogLevel::Warn);
        assert!(mock.media_actions().is_empty());
    }

    #[tokio::test]
    async fn volume_reaches_the_media_adapter_and_is_clamped() {
        let (media, desktop) = recording_desktop(&["Kitchen"]);
        run_with(
            &desktop,
            &request(json!({"player": "Kitchen", "setVolume": 40})),
        )
        .await
        .unwrap();
        run_with(
            &desktop,
            &request(json!({"player": "Kitchen", "setVolume": 150})),
        )
        .await
        .unwrap();
        assert_eq!(
            media.volumes(),
            vec![("Kitchen".to_string(), 40), ("Kitchen".to_string(), 100)]
        );
    }

    #[tokio::test]
    async fn missing_media_adapter_is_an_error() {
        let desktop = Desktop::default();
        let result = run_with(
            &desktop,
            &request(json!({"player": "Kitchen", "action": "Play"})),
        )
        .await;
        assert!(matches!(
            result,
            Err(PluginError::UnsupportedDesktop("media"))
        ));
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(MprisControlPlugin::new().meta(), &roster::MPRISCONTROL);
        assert_eq!(MprisControlFactory.meta(), &roster::MPRISCONTROL);
        assert_eq!(roster::MPRISCONTROL.status, PluginStatus::Implemented);
    }
}
