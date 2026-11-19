//! MPRIS remote plugin: control the phone's media player from the desktop.
//!
//! The peer phone advertises its players with `kdeconnect.mpris` and answers
//! requests sent as `kdeconnect.mpris.request`. This plugin caches that state so
//! a desktop front end can show what the phone is playing, then sends transport
//! and volume requests back. It never touches a local media adapter: the phone
//! owns the players, and the desktop only needs to remember what it was told.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Map, Value};

use crate::roster;

/// Longest accepted player name, in bytes.
///
/// MPRIS identities are short service suffixes. The cap keeps a hostile packet
/// from filling the cache with one oversized key.
const MAX_PLAYER_BYTES: usize = 256;

/// Transport actions the phone accepts, in wire spelling.
const ACTIONS: &[&str] = &[
    "Play",
    "Pause",
    "PlayPause",
    "Stop",
    "Next",
    "Previous",
    "FastForward",
    "Rewind",
];

/// Largest accepted seek offset, in milliseconds.
///
/// A seek is a relative jump. The bound keeps a hostile packet from asking the
/// phone to move a media position by an absurd amount.
const MAX_SEEK_MS: i64 = 24 * 60 * 60 * 1000;

/// What one `kdeconnect.mpris` packet told us about one player.
#[derive(Debug, Clone, Default, PartialEq)]
struct RemotePlayer {
    /// Whether the phone reports the player as playing.
    playing: bool,
    /// Track title, when the phone sent one.
    title: Option<String>,
    /// Track artist, when the phone sent one.
    artist: Option<String>,
    /// Playback position, in milliseconds.
    position_ms: i64,
    /// Track length, in milliseconds.
    length_ms: i64,
    /// Player volume, 0-100.
    volume: Option<u8>,
}

impl RemotePlayer {
    /// Merge the fields a peer sent into this player.
    ///
    /// A packet is partial by design: the phone answers a `requestNowPlaying`
    /// with a subset of the fields, so absent keys keep the cached value.
    fn merge(&mut self, body: &Map<String, Value>) {
        if let Some(playing) = body.get("isPlaying").and_then(Value::as_bool) {
            self.playing = playing;
        }
        if let Some(title) = body.get("title").and_then(Value::as_str) {
            self.title = Some(title.to_string());
        }
        if let Some(artist) = body.get("artist").and_then(Value::as_str) {
            self.artist = Some(artist.to_string());
        }
        if let Some(position) = body.get("pos").and_then(Value::as_i64) {
            self.position_ms = position.max(0);
        }
        if let Some(length) = body.get("length").and_then(Value::as_i64) {
            self.length_ms = length.max(0);
        }
        if let Some(volume) = body.get("volume").and_then(Value::as_i64) {
            self.volume = Some(volume.clamp(0, 100) as u8);
        }
    }
}

/// A request the desktop asked this plugin to send.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outgoing {
    /// Ask for the player list.
    PlayerList,
    /// Ask one player for its now-playing state and volume.
    NowPlaying { player: String },
    /// Run a transport action on one player.
    Action { player: String, action: String },
    /// Set one player's volume, 0-100.
    Volume { player: String, volume: u8 },
    /// Jump one player forward or back by this many milliseconds.
    Seek { player: String, offset_ms: i64 },
}

impl Outgoing {
    /// Render the request as a `kdeconnect.mpris.request` packet.
    fn packet(&self) -> Result<Packet, PluginError> {
        let body = match self {
            Self::PlayerList => json!({ "requestPlayerList": true }),
            Self::NowPlaying { player } => json!({
                "player": player,
                "requestNowPlaying": true,
                "requestVolume": true,
            }),
            Self::Action { player, action } => json!({ "player": player, "action": action }),
            Self::Volume { player, volume } => json!({ "player": player, "setVolume": volume }),
            Self::Seek { player, offset_ms } => json!({ "player": player, "Seek": offset_ms }),
        };
        Packet::new(types::MPRIS_REQUEST, body)
            .map_err(|error| PluginError::InvalidPacket(error.to_string()))
    }
}

/// Why a remote-control request was rejected before it queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteError {
    /// The player name was empty, longer than 256 bytes, or held control
    /// characters.
    InvalidPlayer,
    /// The action is not one the phone accepts.
    UnsupportedAction,
    /// The seek offset was zero or larger than 24 hours.
    InvalidSeek,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPlayer => f.write_str("unusable player name"),
            Self::UnsupportedAction => f.write_str("unsupported action"),
            Self::InvalidSeek => f.write_str("seek offset out of range"),
        }
    }
}

impl std::error::Error for RemoteError {}

fn valid_player(player: &str) -> Result<(), RemoteError> {
    if player.is_empty() || player.len() > MAX_PLAYER_BYTES {
        return Err(RemoteError::InvalidPlayer);
    }
    if player.chars().any(char::is_control) {
        return Err(RemoteError::InvalidPlayer);
    }
    Ok(())
}

/// A phone-to-desktop remote that caches the phone's player state.
#[derive(Debug, Default)]
pub struct MprisRemotePlugin {
    /// Player states, keyed by the phone's player identity.
    players: BTreeMap<String, RemotePlayer>,
    /// Requests queued for the next tick.
    pending: Vec<Outgoing>,
}

impl MprisRemotePlugin {
    /// Create an MPRIS remote plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            players: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    /// Player names the phone has advertised, in sorted order.
    #[must_use]
    pub fn players(&self) -> Vec<String> {
        self.players.keys().cloned().collect()
    }

    /// Refresh the player list and every cached player's state.
    ///
    /// Cached keys already passed validation when they entered the map, so the
    /// per-player requests cannot fail; the method ignores the result to keep
    /// that invariant out of the signature.
    pub fn refresh(&mut self) {
        self.request_player_list();
        let players: Vec<String> = self.players.keys().cloned().collect();
        for player in players {
            drop(self.request_now_playing(&player));
        }
    }

    /// Ask the phone for its player list.
    pub fn request_player_list(&mut self) {
        self.pending.push(Outgoing::PlayerList);
    }

    /// Ask one player for its now-playing state.
    ///
    /// # Errors
    ///
    /// Fails when the player name is empty, oversized or contains control
    /// characters.
    pub fn request_now_playing(&mut self, player: &str) -> Result<(), RemoteError> {
        valid_player(player)?;
        self.pending.push(Outgoing::NowPlaying {
            player: player.to_string(),
        });
        Ok(())
    }

    /// Run a transport action on one player.
    ///
    /// # Errors
    ///
    /// Fails for an unusable player name or an action the phone does not accept.
    pub fn control(&mut self, player: &str, action: &str) -> Result<(), RemoteError> {
        valid_player(player)?;
        let Some(action) = ACTIONS.iter().find(|known| **known == action) else {
            return Err(RemoteError::UnsupportedAction);
        };
        self.pending.push(Outgoing::Action {
            player: player.to_string(),
            action: (*action).to_string(),
        });
        Ok(())
    }

    /// Set one player's volume.
    ///
    /// # Errors
    ///
    /// Fails when the player name is unusable.
    pub fn set_volume(&mut self, player: &str, volume: u8) -> Result<(), RemoteError> {
        valid_player(player)?;
        self.pending.push(Outgoing::Volume {
            player: player.to_string(),
            volume: volume.min(100),
        });
        Ok(())
    }

    /// Seek one player by a relative offset, as the Android client does.
    ///
    /// # Errors
    ///
    /// Fails for an unusable player name or a zero or out-of-range offset.
    pub fn seek(&mut self, player: &str, offset_ms: i64) -> Result<(), RemoteError> {
        valid_player(player)?;
        if offset_ms == 0 || offset_ms.unsigned_abs() > MAX_SEEK_MS as u64 {
            return Err(RemoteError::InvalidSeek);
        }
        self.pending.push(Outgoing::Seek {
            player: player.to_string(),
            offset_ms,
        });
        Ok(())
    }

    /// Number of queued requests.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Apply a `kdeconnect.mpris` body to the cache.
    fn apply(&mut self, packet: &Packet) -> PluginOutput {
        let mut output = PluginOutput::new();
        if let Some(list) = packet.body.get("playerList").and_then(Value::as_array) {
            let mut announced = BTreeSet::new();
            for name in list.iter().filter_map(Value::as_str) {
                if valid_player(name).is_err() {
                    continue;
                }
                self.players.entry(name.to_string()).or_default();
                announced.insert(name.to_string());
            }
            // A player list is authoritative: players missing from it are gone.
            self.players.retain(|name, _| announced.contains(name));
            output = output.with_event(PluginEvent::Log {
                level: LogLevel::Info,
                message: format!("phone reports {} players", announced.len()),
            });
        }
        if let Some(name) = packet.body.get("player").and_then(Value::as_str) {
            if valid_player(name).is_err() {
                return output.with_event(PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: "mpris state ignored: unusable player name".to_string(),
                });
            }
            self.players
                .entry(name.to_string())
                .or_default()
                .merge(&packet.body);
        }
        output
    }
}

#[async_trait]
impl Plugin for MprisRemotePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::MPRISREMOTE
    }

    async fn on_tick(&mut self, _ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        if self.pending.is_empty() {
            return Ok(PluginOutput::new());
        }
        let mut output = PluginOutput::new();
        for request in std::mem::take(&mut self.pending) {
            output = output.with_packet(request.packet()?);
        }
        Ok(output)
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::MPRIS {
            return Ok(PluginOutput::new());
        }
        Ok(self.apply(packet))
    }
}

/// Builds MPRIS remote plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct MprisRemoteFactory;

impl PluginFactory for MprisRemoteFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::MPRISREMOTE
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(MprisRemotePlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginStatus};

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Phone"),
            DeviceType::Phone,
        )
    }

    fn state_packet(body: Value) -> Packet {
        Packet::from_body(types::MPRIS, &body).unwrap()
    }

    async fn feed(plugin: &mut MprisRemotePlugin, packet: &Packet) -> PluginOutput {
        let desktop = MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        plugin.on_packet(&mut ctx, packet).await.unwrap()
    }

    async fn tick(plugin: &mut MprisRemotePlugin) -> PluginOutput {
        let desktop = MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        plugin.on_tick(&mut ctx).await.unwrap()
    }

    #[test]
    fn the_roster_entry_is_implemented_and_matches_the_wire() {
        assert_eq!(roster::MPRISREMOTE.status, PluginStatus::Implemented);
        assert_eq!(roster::MPRISREMOTE.incoming, &[types::MPRIS]);
        assert_eq!(roster::MPRISREMOTE.outgoing, &[types::MPRIS_REQUEST]);
    }

    #[tokio::test]
    async fn a_player_list_replaces_the_cache() {
        let mut plugin = MprisRemotePlugin::new();
        feed(
            &mut plugin,
            &state_packet(json!({ "playerList": ["vlc", "spotify"] })),
        )
        .await;
        assert_eq!(plugin.players(), vec!["spotify", "vlc"]);

        feed(&mut plugin, &state_packet(json!({ "playerList": ["vlc"] }))).await;
        assert_eq!(plugin.players(), vec!["vlc"]);
    }

    #[tokio::test]
    async fn now_playing_state_is_cached_and_merged() {
        let mut plugin = MprisRemotePlugin::new();
        feed(
            &mut plugin,
            &state_packet(json!({
                "player": "vlc",
                "isPlaying": true,
                "title": "Song",
                "artist": "Band",
                "pos": 1000,
                "length": 200_000,
                "volume": 40,
            })),
        )
        .await;

        let vlc = plugin.players.get("vlc").unwrap().clone();
        assert!(vlc.playing);
        assert_eq!(vlc.title.as_deref(), Some("Song"));
        assert_eq!(vlc.artist.as_deref(), Some("Band"));
        assert_eq!(vlc.position_ms, 1000);
        assert_eq!(vlc.length_ms, 200_000);

        // A later, partial packet keeps the fields it did not mention.
        feed(
            &mut plugin,
            &state_packet(json!({ "player": "vlc", "isPlaying": false, "pos": 2000 })),
        )
        .await;
        let vlc = plugin.players.get("vlc").unwrap().clone();
        assert!(!vlc.playing);
        assert_eq!(vlc.title.as_deref(), Some("Song"));
        assert_eq!(vlc.position_ms, 2000);
    }

    #[tokio::test]
    async fn unusable_player_names_are_dropped() {
        let mut plugin = MprisRemotePlugin::new();
        feed(
            &mut plugin,
            &state_packet(json!({ "playerList": ["ok", "", "bad\u{7}name"] })),
        )
        .await;
        assert_eq!(plugin.players(), vec!["ok"]);

        feed(
            &mut plugin,
            &state_packet(json!({ "player": "bad\u{7}", "isPlaying": true })),
        )
        .await;
        assert!(!plugin.players.contains_key("bad\u{7}"));
    }

    #[tokio::test]
    async fn requests_become_packets_on_the_next_tick() {
        let mut plugin = MprisRemotePlugin::new();
        plugin.request_player_list();
        plugin.request_now_playing("vlc").unwrap();
        plugin.control("vlc", "PlayPause").unwrap();
        plugin.set_volume("vlc", 120).unwrap();
        plugin.seek("vlc", -5000).unwrap();
        assert_eq!(plugin.pending(), 5);

        let output = tick(&mut plugin).await;
        assert_eq!(plugin.pending(), 0);
        assert_eq!(output.packets.len(), 5);
        assert!(output
            .packets
            .iter()
            .all(|p| p.ptype == types::MPRIS_REQUEST));
        assert_eq!(
            output.packets[0].body,
            *json!({ "requestPlayerList": true }).as_object().unwrap()
        );
        assert_eq!(
            output.packets[1].body.get("player"),
            Some(&json!("vlc")),
            "the now-playing request names the player"
        );
        assert_eq!(
            output.packets[1].body.get("requestVolume"),
            Some(&json!(true))
        );
        assert_eq!(
            output.packets[2].body.get("action"),
            Some(&json!("PlayPause"))
        );
        // Volume above 100 is clamped to the wire range.
        assert_eq!(output.packets[3].body.get("setVolume"), Some(&json!(100)));
        // Seek keeps the signed millisecond offset and the wire spelling.
        assert_eq!(output.packets[4].body.get("Seek"), Some(&json!(-5000)));

        // An empty queue sends nothing.
        assert!(tick(&mut plugin).await.packets.is_empty());
    }

    #[test]
    fn bad_requests_are_rejected_before_they_queue() {
        let mut plugin = MprisRemotePlugin::new();
        assert!(plugin.request_now_playing("").is_err());
        assert!(plugin.control("vlc", "Shuffle").is_err());
        assert!(plugin
            .set_volume(&"x".repeat(MAX_PLAYER_BYTES + 1), 10)
            .is_err());
        assert!(plugin.seek("vlc", 0).is_err());
        assert!(plugin.seek("vlc", MAX_SEEK_MS + 1).is_err());
        assert_eq!(plugin.pending(), 0);
    }

    #[tokio::test]
    async fn connecting_asks_for_the_player_list() {
        let mut plugin = MprisRemotePlugin::new();
        assert!(tick(&mut plugin).await.packets.is_empty());

        // Upstream refreshes on connect through its D-Bus surface; the plugin
        // exposes that as an explicit call and sends it on the next tick.
        plugin.refresh();
        let output = tick(&mut plugin).await;
        assert_eq!(output.packets.len(), 1);
        assert_eq!(
            output.packets[0].body.get("requestPlayerList"),
            Some(&json!(true))
        );
    }

    #[tokio::test]
    async fn other_packets_are_ignored() {
        let mut plugin = MprisRemotePlugin::new();
        let packet =
            Packet::from_body(types::MPRIS_REQUEST, &json!({ "requestPlayerList": true })).unwrap();
        assert!(feed(&mut plugin, &packet).await.packets.is_empty());
        assert!(plugin.players().is_empty());
    }
}
