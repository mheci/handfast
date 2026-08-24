//! Pause-music-on-call logic (`pause_music`).
//!
//! A pure transformer with no IO of its own: it learns player names from
//! [`TYPE_MPRIS`] state packets flowing through the daemon, and when told a
//! call is active ([`pause_all`](PauseMusicPlugin::pause_all)) emits
//! [`TYPE_MPRIS_REQUEST`] packets pausing every known player.
//! [`resume_all`](PauseMusicPlugin::resume_all) replays `Play` actions for
//! exactly the players this plugin paused. Desktop MPRIS binding arrives in
//! Phase 3; until then callers drive pause/resume manually.

use std::collections::HashSet;

use handfast_protocol::{Packet, TYPE_MPRIS, TYPE_MPRIS_REQUEST};
use serde_json::Value;

use crate::{meta, Plugin, PluginFactory, PluginMeta};

/// Remembers players seen on the wire and which ones we paused.
#[derive(Debug, Default)]
pub struct PauseMusicPlugin {
    known_players: HashSet<String>,
    paused_players: HashSet<String>,
}

impl PauseMusicPlugin {
    /// Create a plugin instance that has seen no players yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Player names learned from passing `kdeconnect.mpris` traffic,
    /// sorted for deterministic presentation.
    #[must_use]
    pub fn known_players(&self) -> Vec<String> {
        let mut names: Vec<String> = self.known_players.iter().cloned().collect();
        names.sort_unstable();
        names
    }

    /// Emit `Pause` requests for every known player and remember them so a
    /// later [`Self::resume_all`] restores precisely this set.
    pub fn pause_all(&mut self) -> Vec<Packet> {
        let targets = self.known_players();
        let mut packets = Vec::with_capacity(targets.len());
        for name in &targets {
            packets.push(transport_request(name, "Pause"));
            self.paused_players.insert(name.clone());
        }
        tracing::debug!(
            plugin = meta::PAUSE_MUSIC.name,
            paused = packets.len(),
            "pausing known players"
        );
        packets
    }

    /// Emit `Play` requests for the players previously paused by
    /// [`Self::pause_all`], clearing the paused set. Players that vanished
    /// or were never paused are not resumed.
    pub fn resume_all(&mut self) -> Vec<Packet> {
        let targets: Vec<String> = {
            let mut names: Vec<String> = self.paused_players.iter().cloned().collect();
            names.sort_unstable();
            names
        };
        self.paused_players.clear();
        let packets: Vec<Packet> = targets
            .iter()
            .map(|name| transport_request(name, "Play"))
            .collect();
        tracing::debug!(
            plugin = meta::PAUSE_MUSIC.name,
            resumed = packets.len(),
            "resuming previously paused players"
        );
        packets
    }

    fn learn_players(&mut self, body: &Value) {
        if let Some(list) = body.get("playerList").and_then(Value::as_array) {
            for entry in list {
                if let Some(name) = entry.get("name").and_then(Value::as_str) {
                    self.remember_player(name);
                }
            }
        }
        if let Some(name) = body.get("player").and_then(Value::as_str) {
            self.remember_player(name);
        }
    }

    fn remember_player(&mut self, name: &str) {
        if name.is_empty() {
            tracing::debug!(
                plugin = meta::PAUSE_MUSIC.name,
                "ignoring empty player name"
            );
            return;
        }
        if self.known_players.insert(name.to_owned()) {
            tracing::debug!(
                plugin = meta::PAUSE_MUSIC.name,
                player = name,
                "learned player"
            );
        }
    }
}

impl Plugin for PauseMusicPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &meta::PAUSE_MUSIC
    }

    fn handle(&mut self, pkt: &Packet) -> Vec<Packet> {
        if pkt.ty() != TYPE_MPRIS {
            tracing::debug!(
                plugin = meta::PAUSE_MUSIC.name,
                got = pkt.ty(),
                "ignoring foreign packet"
            );
            return Vec::new();
        }
        self.learn_players(&pkt.body);
        Vec::new()
    }
}

/// Builds one `kdeconnect.mpris.request` transport command packet.
fn transport_request(player: &str, action: &str) -> Packet {
    Packet::new(
        TYPE_MPRIS_REQUEST,
        serde_json::json!({ "player": player, "action": action }),
    )
}

/// Builds [`PauseMusicPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct PauseMusicFactory;

impl PluginFactory for PauseMusicFactory {
    fn meta(&self) -> &'static PluginMeta {
        &meta::PAUSE_MUSIC
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(PauseMusicPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn player_list(players: &[&str]) -> Value {
        json!({"playerList": players
            .iter()
            .map(|name| json!({"name": name}))
            .collect::<Vec<_>>()})
    }

    #[test]
    fn learns_players_from_both_packet_shapes() {
        let mut plugin = PauseMusicPlugin::new();
        assert!(plugin.known_players().is_empty());
        plugin.handle(&Packet::new(TYPE_MPRIS, player_list(&["spotify", "vlc"])));
        plugin.handle(&Packet::new(
            TYPE_MPRIS,
            json!({"player": "mpv", "isPlaying": true}),
        ));
        assert_eq!(plugin.known_players(), ["mpv", "spotify", "vlc"]);
    }

    #[test]
    fn ignores_malformed_mpris_bodies() {
        let mut plugin = PauseMusicPlugin::new();
        for body in [
            Value::Null,
            json!("players"),
            json!([]),
            json!({}),
            json!({"playerList": "nope"}),
            json!({"playerList": [1, null, {}]}),
            json!({"playerList": [{"name": 7}]}),
            json!({"player": true}),
            json!({"player": ""}),
        ] {
            plugin.handle(&Packet::new(TYPE_MPRIS, body.clone()));
            assert!(plugin.known_players().is_empty(), "body {body} leaked in");
        }
    }

    #[test]
    fn ignores_foreign_packet_types_without_learning() {
        let mut plugin = PauseMusicPlugin::new();
        assert!(plugin
            .handle(&Packet::new("kdeconnect.ping", player_list(&["spotify"])))
            .is_empty());
        assert!(plugin
            .handle(&Packet::new(TYPE_MPRIS_REQUEST, player_list(&["spotify"])))
            .is_empty());
        assert!(plugin.known_players().is_empty());
    }

    #[test]
    fn pause_emits_pause_for_every_known_player() {
        let mut plugin = PauseMusicPlugin::new();
        assert!(plugin.pause_all().is_empty(), "nothing known yet");

        plugin.handle(&Packet::new(TYPE_MPRIS, player_list(&["vlc", "spotify"])));
        let packets = plugin.pause_all();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].ty(), TYPE_MPRIS_REQUEST);
        assert_eq!(packets[0].body["player"], json!("spotify"));
        assert_eq!(packets[0].body["action"], json!("Pause"));
        assert_eq!(packets[1].body["player"], json!("vlc"));
        assert_eq!(packets[1].body["action"], json!("Pause"));
    }

    #[test]
    fn resume_replays_only_previously_paused_and_clears_set() {
        let mut plugin = PauseMusicPlugin::new();
        // Only spotify was known when the call started.
        plugin.handle(&Packet::new(TYPE_MPRIS, player_list(&["spotify"])));
        assert_eq!(plugin.pause_all().len(), 1);

        // mpv/vlc show up mid-call; a resume must not resurrect them.
        plugin.handle(&Packet::new(TYPE_MPRIS, player_list(&["mpv", "vlc"])));
        let resumed = plugin.resume_all();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].ty(), TYPE_MPRIS_REQUEST);
        assert_eq!(
            resumed[0].body,
            json!({"player": "spotify", "action": "Play"})
        );

        // The set drains: a second resume resumes nothing...
        assert!(plugin.resume_all().is_empty());
        // ...but knowledge of players persists.
        assert_eq!(plugin.known_players(), ["mpv", "spotify", "vlc"]);
    }

    #[test]
    fn repeated_pauses_collapse_into_single_resume() {
        let mut plugin = PauseMusicPlugin::new();
        plugin.handle(&Packet::new(TYPE_MPRIS, player_list(&["spotify"])));
        assert_eq!(plugin.pause_all().len(), 1);
        assert_eq!(plugin.pause_all().len(), 1);
        let resumed = plugin.resume_all();
        assert_eq!(resumed.len(), 1);
        assert_eq!(
            resumed[0].body,
            json!({"player": "spotify", "action": "Play"})
        );
    }

    #[test]
    fn handle_is_always_silent() {
        let mut plugin = PauseMusicPlugin::new();
        assert!(plugin
            .handle(&Packet::new(TYPE_MPRIS, player_list(&["spotify"])))
            .is_empty());
    }

    #[test]
    fn factory_builds_fresh_instances() {
        let a = PauseMusicFactory.create();
        let b = PauseMusicFactory.create();
        assert_eq!(a.meta().name, "pause_music");
        assert_eq!(b.meta().name, "pause_music");
    }
}
