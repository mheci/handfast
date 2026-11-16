//! Remote MPRIS plugin.
//!
//! The desktop keeps the media players a peer reports and sends transport,
//! volume and seek requests back. The peer owns its playback; this plugin never
//! touches a local player.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput, RemoteMediaAction, RemotePlayer,
};
use serde_json::{json, Value};

use crate::roster;

const MAX_PLAYERS: usize = 32;
const MAX_NAME_BYTES: usize = 256;
const MAX_TEXT_BYTES: usize = 1024;
const MAX_TIME_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Whether `player` is a player name a peer may use in a request.
#[must_use]
pub fn valid_player_name(player: &str) -> bool {
    !player.is_empty() && player.len() <= MAX_NAME_BYTES && !player.chars().any(char::is_control)
}

fn text_field(packet: &Packet, name: &str) -> Result<Option<String>, String> {
    match packet.body.get(name) {
        None => Ok(None),
        Some(Value::String(text))
            if text.len() <= MAX_TEXT_BYTES && !text.chars().any(char::is_control) =>
        {
            Ok(Some(text.clone()))
        }
        Some(_) => Err(format!("mpris {name} is not a valid string")),
    }
}

fn bool_field(packet: &Packet, name: &str) -> Result<Option<bool>, String> {
    match packet.body.get(name) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| format!("mpris {name} is not a boolean")),
    }
}

fn time_field(packet: &Packet, name: &str) -> Result<Option<i64>, String> {
    match packet.body.get(name) {
        None => Ok(None),
        Some(value) => match value.as_i64() {
            Some(value) if (0..=MAX_TIME_MS).contains(&value) => Ok(Some(value)),
            _ => Err(format!("mpris {name} is out of range")),
        },
    }
}

fn volume_field(packet: &Packet) -> Result<Option<u8>, String> {
    match packet.body.get("volume") {
        None => Ok(None),
        Some(value) => match value.as_u64().and_then(|value| u8::try_from(value).ok()) {
            Some(value) if value <= 100 => Ok(Some(value)),
            _ => Err("mpris volume is out of range".to_string()),
        },
    }
}

fn log(level: LogLevel, message: impl Into<String>) -> PluginEvent {
    PluginEvent::Log {
        level,
        message: message.into(),
    }
}

/// Tracks the media players a peer reports.
#[derive(Debug, Default)]
pub struct MprisRemotePlugin {
    players: BTreeMap<String, RemotePlayer>,
}

impl MprisRemotePlugin {
    fn model(&self) -> PluginEvent {
        PluginEvent::RemotePlayers {
            players: self.players.values().cloned().collect(),
        }
    }

    fn apply_player_list(
        &mut self,
        packet: &Packet,
        requests: &mut Vec<Packet>,
        warnings: &mut Vec<PluginEvent>,
    ) {
        let Some(list) = packet.body.get("playerList").and_then(Value::as_array) else {
            warnings.push(log(LogLevel::Warn, "mpris player list is not an array"));
            return;
        };
        let mut names = BTreeSet::new();
        for value in list {
            match value.as_str() {
                Some(name) if valid_player_name(name) && names.len() < MAX_PLAYERS => {
                    names.insert(name.to_string());
                }
                Some(name) if valid_player_name(name) => {
                    warnings.push(log(
                        LogLevel::Warn,
                        format!("skipped mpris player over the cap: {name}"),
                    ));
                }
                _ => warnings.push(log(LogLevel::Warn, "skipped an invalid mpris player name")),
            }
        }
        self.players.retain(|name, _| names.contains(name));
        for name in &names {
            if self.players.contains_key(name) {
                continue;
            }
            let player = RemotePlayer {
                player: name.clone(),
                volume: 50,
                ..RemotePlayer::default()
            };
            self.players.insert(name.clone(), player);
            match request_player_status(name) {
                Ok(packet) => requests.push(packet),
                Err(error) => warnings.push(log(
                    LogLevel::Warn,
                    format!("failed to build an mpris status request: {error}"),
                )),
            }
        }
    }

    fn apply_state(&mut self, player: &str, packet: &Packet) -> Result<(), String> {
        let mut model = self
            .players
            .get(player)
            .cloned()
            .unwrap_or_else(|| RemotePlayer {
                player: player.to_string(),
                volume: 50,
                ..RemotePlayer::default()
            });
        model.player = player.to_string();
        if let Some(text) = text_field(packet, "title")? {
            model.title = text;
        }
        if let Some(text) = text_field(packet, "artist")? {
            model.artist = text;
        }
        if let Some(text) = text_field(packet, "album")? {
            model.album = text;
        }
        if let Some(text) = text_field(packet, "albumArtUrl")? {
            model.album_art_url = text;
        }
        if let Some(value) = time_field(packet, "length")? {
            model.length_ms = value;
        }
        if let Some(value) = time_field(packet, "pos")? {
            model.position_ms = value;
        }
        if let Some(value) = volume_field(packet)? {
            model.volume = value;
        }
        if let Some(value) = bool_field(packet, "isPlaying")? {
            model.playing = value;
        }
        if let Some(value) = bool_field(packet, "canSeek")? {
            model.can_seek = value;
        }
        if let Some(value) = bool_field(packet, "canPlay")? {
            model.can_play = value;
        }
        if let Some(value) = bool_field(packet, "canPause")? {
            model.can_pause = value;
        }
        if let Some(value) = bool_field(packet, "canGoPrevious")? {
            model.can_go_previous = value;
        }
        if let Some(value) = bool_field(packet, "canGoNext")? {
            model.can_go_next = value;
        }
        if let Some(value) = bool_field(packet, "canFastForward")? {
            model.can_fast_forward = value;
        }
        if let Some(value) = bool_field(packet, "canRewind")? {
            model.can_rewind = value;
        }
        self.players.insert(player.to_string(), model);
        Ok(())
    }
}

#[async_trait]
impl Plugin for MprisRemotePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::MPRISREMOTE
    }

    async fn on_connect(
        &mut self,
        _ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        let packet =
            request_player_list().map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
        Ok(PluginOutput::new().with_packet(packet))
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::MPRIS {
            return Ok(PluginOutput::new());
        }
        if packet
            .body
            .get("transferringAlbumArt")
            .and_then(Value::as_bool)
            == Some(true)
        {
            // shortcut: album art payloads are not fetched, upgrade when the
            // session exposes album-art payload receives.
            return Ok(PluginOutput::new());
        }
        let mut requests = Vec::new();
        let mut warnings = Vec::new();
        if packet.body.contains_key("playerList") {
            self.apply_player_list(packet, &mut requests, &mut warnings);
        }
        if let Some(value) = packet.body.get("player") {
            match value.as_str() {
                Some(player) if valid_player_name(player) => {
                    if let Err(message) = self.apply_state(player, packet) {
                        warnings.push(log(LogLevel::Warn, message));
                    }
                }
                _ => warnings.push(log(
                    LogLevel::Warn,
                    "mpris packet with an invalid player name",
                )),
            }
        }
        let mut output = PluginOutput::new().with_event(self.model());
        for packet in requests {
            output = output.with_packet(packet);
        }
        output.events.extend(warnings);
        Ok(output)
    }
}

/// Creates one player model per device session.
#[derive(Debug, Clone, Copy, Default)]
pub struct MprisRemoteFactory;

impl PluginFactory for MprisRemoteFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::MPRISREMOTE
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(MprisRemotePlugin::default())
    }
}

/// Build a request for the peer's player list.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn request_player_list() -> Result<Packet, ProtoError> {
    Packet::new(types::MPRIS_REQUEST, json!({ "requestPlayerList": true }))
}

/// Build a request for one player's state.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn request_player_status(player: &str) -> Result<Packet, ProtoError> {
    Packet::new(
        types::MPRIS_REQUEST,
        json!({
            "player": player,
            "requestNowPlaying": true,
            "requestVolume": true,
        }),
    )
}

/// Build a transport action request.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn action_request(player: &str, action: RemoteMediaAction) -> Result<Packet, ProtoError> {
    Packet::new(
        types::MPRIS_REQUEST,
        json!({ "player": player, "action": action.as_str() }),
    )
}

/// Build a request that sets one player's volume percentage.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn volume_request(player: &str, volume: u8) -> Result<Packet, ProtoError> {
    Packet::new(
        types::MPRIS_REQUEST,
        json!({ "player": player, "setVolume": volume.min(100) }),
    )
}

/// Build a request that seeks one player by a relative offset in microseconds.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn seek_request(player: &str, offset_us: i64) -> Result<Packet, ProtoError> {
    Packet::new(
        types::MPRIS_REQUEST,
        json!({ "player": player, "Seek": offset_us }),
    )
}

/// Build a request that seeks one player to an absolute position.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn set_position_request(player: &str, position_ms: i64) -> Result<Packet, ProtoError> {
    Packet::new(
        types::MPRIS_REQUEST,
        json!({ "player": player, "SetPosition": position_ms }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn run(plugin: &mut MprisRemotePlugin, packet: Packet) -> PluginOutput {
        let device = device();
        let desktop = handfast_desktop::Desktop::default();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    async fn connect(plugin: &mut MprisRemotePlugin) -> PluginOutput {
        let device = device();
        let desktop = handfast_desktop::Desktop::default();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        plugin.on_connect(&mut ctx).await.unwrap()
    }

    fn packet(body: Value) -> Packet {
        Packet::from_body(types::MPRIS, &body).unwrap()
    }

    fn players(output: &PluginOutput) -> &[RemotePlayer] {
        output
            .events
            .iter()
            .find_map(|event| match event {
                PluginEvent::RemotePlayers { players } => Some(players.as_slice()),
                _ => None,
            })
            .expect("a player event")
    }

    #[tokio::test]
    async fn connect_requests_the_player_list() {
        let mut plugin = MprisRemotePlugin::default();
        let output = connect(&mut plugin).await;

        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::MPRIS_REQUEST);
        assert_eq!(output.packets[0].body["requestPlayerList"], true);
    }

    #[tokio::test]
    async fn a_player_list_adds_players_and_requests_status() {
        let mut plugin = MprisRemotePlugin::default();
        let output = run(
            &mut plugin,
            packet(json!({"playerList": ["mpv", "spotify"]})),
        )
        .await;

        assert_eq!(players(&output).len(), 2);
        assert_eq!(players(&output)[0].player, "mpv");
        assert_eq!(players(&output)[1].player, "spotify");
        assert_eq!(output.packets.len(), 2);
        for packet in &output.packets {
            assert_eq!(packet.ptype, types::MPRIS_REQUEST);
            assert_eq!(packet.body["requestNowPlaying"], true);
            assert_eq!(packet.body["requestVolume"], true);
        }
    }

    #[tokio::test]
    async fn a_player_list_prunes_removed_players() {
        let mut plugin = MprisRemotePlugin::default();
        run(
            &mut plugin,
            packet(json!({"playerList": ["mpv", "spotify"]})),
        )
        .await;

        let output = run(&mut plugin, packet(json!({"playerList": ["spotify"]}))).await;

        assert_eq!(players(&output).len(), 1);
        assert_eq!(players(&output)[0].player, "spotify");
        assert!(output.packets.is_empty());
    }

    #[tokio::test]
    async fn a_state_packet_updates_the_player() {
        let mut plugin = MprisRemotePlugin::default();
        let output = run(
            &mut plugin,
            packet(json!({
                "player": "mpv",
                "title": "Song",
                "artist": "Band",
                "album": "Record",
                "albumArtUrl": "https://example.com/art.jpg",
                "length": 300_000,
                "pos": 1_200,
                "volume": 40,
                "isPlaying": true,
                "canSeek": true,
                "canGoNext": true,
            })),
        )
        .await;

        let player = &players(&output)[0];
        assert_eq!(player.title, "Song");
        assert_eq!(player.artist, "Band");
        assert_eq!(player.album, "Record");
        assert_eq!(player.album_art_url, "https://example.com/art.jpg");
        assert_eq!(player.length_ms, 300_000);
        assert_eq!(player.position_ms, 1_200);
        assert_eq!(player.volume, 40);
        assert!(player.playing);
        assert!(player.can_seek);
        assert!(player.can_go_next);
        assert!(!player.can_pause);
    }

    #[tokio::test]
    async fn a_state_packet_keeps_absent_fields() {
        let mut plugin = MprisRemotePlugin::default();
        run(
            &mut plugin,
            packet(json!({"player": "mpv", "title": "Song", "volume": 40})),
        )
        .await;

        let output = run(
            &mut plugin,
            packet(json!({"player": "mpv", "isPlaying": true})),
        )
        .await;

        let player = &players(&output)[0];
        assert_eq!(player.title, "Song");
        assert_eq!(player.volume, 40);
        assert!(player.playing);
    }

    #[tokio::test]
    async fn an_invalid_player_name_is_a_warning() {
        let mut plugin = MprisRemotePlugin::default();
        let output = run(&mut plugin, packet(json!({"player": "bad\u{0}name"}))).await;

        assert!(matches!(
            output.events.last(),
            Some(PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn an_out_of_range_position_is_a_warning() {
        let mut plugin = MprisRemotePlugin::default();
        let output = run(&mut plugin, packet(json!({"player": "mpv", "pos": -1}))).await;

        assert!(matches!(
            output.events.last(),
            Some(PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn an_album_art_payload_is_ignored() {
        let mut plugin = MprisRemotePlugin::default();
        let output = run(
            &mut plugin,
            packet(json!({"transferringAlbumArt": true, "player": "mpv"})),
        )
        .await;

        assert!(output.events.is_empty());
        assert!(output.packets.is_empty());
    }

    #[test]
    fn requests_carry_the_fields() {
        let list = request_player_list().unwrap();
        assert_eq!(list.body["requestPlayerList"], true);

        let status = request_player_status("mpv").unwrap();
        assert_eq!(status.body["player"], "mpv");
        assert_eq!(status.body["requestNowPlaying"], true);
        assert_eq!(status.body["requestVolume"], true);

        let action = action_request("mpv", RemoteMediaAction::Next).unwrap();
        assert_eq!(action.body["player"], "mpv");
        assert_eq!(action.body["action"], "Next");

        let volume = volume_request("mpv", 140).unwrap();
        assert_eq!(volume.body["setVolume"], 100);

        let seek = seek_request("mpv", -5_000).unwrap();
        assert_eq!(seek.body["Seek"], -5_000);

        let position = set_position_request("mpv", 42_000).unwrap();
        assert_eq!(position.body["SetPosition"], 42_000);
    }

    #[test]
    fn player_names_are_validated() {
        assert!(valid_player_name("org.mpris.MediaPlayer2.vlc"));
        assert!(!valid_player_name(""));
        assert!(!valid_player_name("bad\nname"));
        assert!(!valid_player_name(&"a".repeat(MAX_NAME_BYTES + 1)));
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(&roster::MPRISREMOTE, MprisRemotePlugin::default().meta());
        assert_eq!(&roster::MPRISREMOTE, MprisRemoteFactory.meta());
        assert_eq!(
            roster::MPRISREMOTE.status,
            handfast_runtime::PluginStatus::Implemented
        );
    }
}
