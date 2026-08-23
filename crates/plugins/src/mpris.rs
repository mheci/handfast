//! Fully implemented MPRIS media transport control (Phase-1 parity subset).
//!
//! Consumes inbound [`TYPE_MPRIS_REQUEST`] packets whose body carries a
//! `"player"` name and an `"action"` string. Valid requests are answered with
//! a confirmed echo packet (`ack: true`) carrying the resolved player and the
//! canonical action. Requests naming the pseudo-player `"auto"` are resolved
//! against the most recently seen concrete player name. Anything else —
//! foreign packet types, missing or non-string fields, unknown actions,
//! unresolvable `"auto"` targets — yields no replies.

use handfast_protocol::{Packet, TYPE_MPRIS_REQUEST};

use crate::{meta, Plugin, PluginFactory, PluginMeta};

/// Pseudo-player name requesting resolution via [`MprisPlugin`]'s
/// last-observed-player memory.
const AUTO_PLAYER: &str = "auto";

/// A validated MPRIS transport action.
///
/// String forms match the wire protocol used by upstream kdeconnect-kde
/// exactly (they are case-sensitive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MprisAction {
    /// Start playback.
    Play,
    /// Suspend playback.
    Pause,
    /// Toggle between playing and paused.
    PlayPause,
    /// Halt playback and reset position.
    Stop,
    /// Skip to the next track.
    Next,
    /// Return to the previous track.
    Previous,
}

impl MprisAction {
    /// Canonical wire-format name of this action.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Play => "Play",
            Self::Pause => "Pause",
            Self::PlayPause => "PlayPause",
            Self::Stop => "Stop",
            Self::Next => "Next",
            Self::Previous => "Previous",
        }
    }

    /// Parses a wire-format action string, returning [`None`] for anything
    /// outside the six canonical names.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Play" => Some(Self::Play),
            "Pause" => Some(Self::Pause),
            "PlayPause" => Some(Self::PlayPause),
            "Stop" => Some(Self::Stop),
            "Next" => Some(Self::Next),
            "Previous" => Some(Self::Previous),
            _ => None,
        }
    }
}

/// Answers validated `kdeconnect.mpris.request` transport commands.
#[derive(Debug)]
pub struct MprisPlugin {
    last_known_player: Option<String>,
}

impl MprisPlugin {
    /// Create a plugin instance with no players observed yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_known_player: None,
        }
    }

    /// Most recently requested concrete player name, used to resolve
    /// `"auto"` targets; empty until such a request arrives.
    #[must_use]
    pub fn last_known_player(&self) -> Option<&str> {
        self.last_known_player.as_deref()
    }
}

impl Default for MprisPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl MprisPlugin {
    fn handle_request(&mut self, body: &serde_json::Value) -> Vec<Packet> {
        let Some(player) = body.get("player").and_then(serde_json::Value::as_str) else {
            tracing::debug!(plugin = meta::MPRIS.name, "request missing string player");
            return Vec::new();
        };
        let Some(raw_action) = body.get("action").and_then(serde_json::Value::as_str) else {
            tracing::debug!(plugin = meta::MPRIS.name, "request missing string action");
            return Vec::new();
        };
        let Some(action) = MprisAction::parse(raw_action) else {
            tracing::debug!(
                plugin = meta::MPRIS.name,
                action = raw_action,
                "rejecting unknown action"
            );
            return Vec::new();
        };

        let resolved = if player == AUTO_PLAYER {
            match self.last_known_player.as_deref() {
                Some(known) => known.to_owned(),
                None => {
                    tracing::debug!(
                        plugin = meta::MPRIS.name,
                        "auto target with no known player"
                    );
                    return Vec::new();
                }
            }
        } else {
            player.to_owned()
        };

        self.last_known_player = Some(resolved.clone());
        tracing::debug!(
            plugin = meta::MPRIS.name,
            player = %resolved,
            action = action.as_str(),
            "acking mpris request"
        );
        vec![Packet::new(
            TYPE_MPRIS_REQUEST,
            serde_json::json!({
                "player": resolved,
                "action": action.as_str(),
                "ack": true,
            }),
        )]
    }
}

impl Plugin for MprisPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &meta::MPRIS
    }

    fn handle(&mut self, pkt: &Packet) -> Vec<Packet> {
        if pkt.ty() != TYPE_MPRIS_REQUEST {
            tracing::debug!(
                plugin = meta::MPRIS.name,
                got = pkt.ty(),
                "ignoring foreign packet"
            );
            return Vec::new();
        }
        self.handle_request(&pkt.body)
    }
}

/// Builds [`MprisPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct MprisFactory;

impl PluginFactory for MprisFactory {
    fn meta(&self) -> &'static PluginMeta {
        &meta::MPRIS
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(MprisPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(player: &str, action: &str) -> Packet {
        Packet::new(
            TYPE_MPRIS_REQUEST,
            json!({"player": player, "action": action}),
        )
    }

    #[test]
    fn parses_every_valid_action_and_echoes_with_ack() {
        let cases = [
            ("Play", MprisAction::Play),
            ("Pause", MprisAction::Pause),
            ("PlayPause", MprisAction::PlayPause),
            ("Stop", MprisAction::Stop),
            ("Next", MprisAction::Next),
            ("Previous", MprisAction::Previous),
        ];
        for (raw, expected) in cases {
            assert_eq!(MprisAction::parse(raw), Some(expected));
            assert_eq!(expected.as_str(), raw);
        }

        let mut plugin = MprisPlugin::new();
        let replies = plugin.handle(&request("Spotify", "PlayPause"));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].ty(), TYPE_MPRIS_REQUEST);
        assert_eq!(replies[0].body["player"], "Spotify");
        assert_eq!(replies[0].body["action"], "PlayPause");
        assert_eq!(replies[0].body["ack"], true);
        assert_eq!(plugin.last_known_player(), Some("Spotify"));
    }

    #[test]
    fn invalid_action_yields_no_replies_and_no_state_change() {
        let mut plugin = MprisPlugin::new();
        for bad in ["Loud", "play", "", "PLAY"] {
            assert!(plugin.handle(&request("mpv", bad)).is_empty(), "{bad}");
        }
        assert!(plugin
            .handle(&Packet::new(TYPE_MPRIS_REQUEST, json!({"player": "mpv"})))
            .is_empty());
        assert!(plugin
            .handle(&Packet::new(
                TYPE_MPRIS_REQUEST,
                json!({"player": 7, "action": "Play"})
            ))
            .is_empty());
        assert!(plugin.last_known_player().is_none());
    }

    #[test]
    fn auto_resolves_to_last_known_player() {
        let mut plugin = MprisPlugin::new();
        assert!(plugin.handle(&request("mpv", "Play")).len() == 1);
        let replies = plugin.handle(&request("auto", "Pause"));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].body["player"], "mpv");
        assert_eq!(replies[0].body["action"], "Pause");
        assert_eq!(replies[0].body["ack"], true);
        assert_eq!(plugin.last_known_player(), Some("mpv"));
    }

    #[test]
    fn unknown_players_are_rejected() {
        let mut plugin = MprisPlugin::new();
        assert!(plugin.handle(&request("auto", "Play")).is_empty());
        assert!(plugin.last_known_player().is_none());
        assert!(plugin
            .handle(&Packet::new("kdeconnect.mpris", json!({"player": "mpv"})))
            .is_empty());
        assert!(plugin
            .handle(&Packet::new("kdeconnect.battery", json!({})))
            .is_empty());
    }

    #[test]
    fn factory_builds_fresh_instances() {
        let a = MprisFactory.create();
        let b = MprisFactory.create();
        assert_eq!(a.meta().name, "mpris");
        assert_eq!(b.meta().name, "mpris");
    }
}
