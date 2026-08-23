//! Battery status plugin (Phase-1 parity with the upstream wire format).
//!
//! Two responsibilities:
//!
//! * Answering `kdeconnect.battery.request` probes from the phone with a
//!   report built from the local power supplies. This phase reads
//!   `/sys/class/power_supply/` directly through `std::fs` (no D-Bus/UPower
//!   bridge yet); when nothing usable is found the well-known fallback level
//!   of [`FALLBACK_LEVEL`] is reported instead.
//! * Recording `kdeconnect.battery` reports pushed by the phone so UI
//!   surfaces can show remote charge state via
//!   [`BatteryPlugin::last_state`], optionally mirrored into a
//!   [`BatteryCache`] shared across the process and keyed by device id.
//!
//! Outbound reports use the upstream body shape
//! `{"currentCharge": <0-100>, "isCharging": bool, "thresholdEvent": 0}`.
//! Low-battery threshold notifications are deferred to the Phase-3 desktop
//! integration and are always reported as `0` here.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use handfast_protocol::{Packet, TYPE_BATTERY, TYPE_BATTERY_REQUEST};
use serde_json::Value;

use crate::{meta, Plugin, PluginFactory, PluginMeta};

/// Charge percentage reported when no usable local power supply is found.
pub const FALLBACK_LEVEL: u8 = 50;

/// Linux sysfs root enumerating power supplies.
const POWER_SUPPLY_ROOT: &str = "/sys/class/power_supply/";

/// Latest known charge state of one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryState {
    /// Remaining charge, 0-100 percent.
    level: u8,
    /// Whether the device is plugged in and charging.
    charging: bool,
    /// Wall-clock time the state was observed.
    reported_at: SystemTime,
}

impl BatteryState {
    /// Remaining charge, 0-100 percent.
    #[must_use]
    pub fn level(self) -> u8 {
        self.level
    }

    /// Whether the device is plugged in and charging.
    #[must_use]
    pub fn charging(self) -> bool {
        self.charging
    }

    /// Wall-clock time the state was observed.
    #[must_use]
    pub fn reported_at(self) -> SystemTime {
        self.reported_at
    }
}

/// Builds the upstream-shaped `kdeconnect.battery` report packet.
///
/// Levels above 100 are clamped defensively; `thresholdEvent` is always `0`
/// until the Phase-3 low-battery notification work lands.
#[must_use]
pub fn build_report(level: u8, charging: bool) -> Packet {
    Packet::new(
        TYPE_BATTERY,
        serde_json::json!({
            "currentCharge": level.min(100),
            "isCharging": charging,
            "thresholdEvent": 0,
        }),
    )
}

/// Neutral report used when local hardware probing fails entirely.
#[must_use]
pub fn fallback_state() -> BatteryState {
    BatteryState {
        level: FALLBACK_LEVEL,
        charging: false,
        reported_at: SystemTime::now(),
    }
}

/// Reads the best local battery from `/sys/class/power_supply/`.
///
/// Supplies named `BAT*` are preferred, then any supply advertising
/// `"type": "Battery"`; the scan order is otherwise deterministic. Returns
/// `None` when the directory is unreadable or no battery exposes a plausible
/// `capacity` (the common case on non-Linux hosts and VMs).
#[must_use]
pub fn read_local_battery() -> Option<BatteryState> {
    let mut supplies: Vec<PathBuf> = fs::read_dir(POWER_SUPPLY_ROOT)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    supplies.sort_by_key(|path| {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        (!name.starts_with("BAT"), name.to_owned())
    });
    for path in &supplies {
        if let Some(state) = read_supply(path) {
            return Some(state);
        }
    }
    None
}

/// Parses one sysfs power-supply directory into a [`BatteryState`].
fn read_supply(path: &Path) -> Option<BatteryState> {
    let kind = fs::read_to_string(path.join("type")).ok()?;
    if kind.trim() != "Battery" {
        return None;
    }
    let capacity = fs::read_to_string(path.join("capacity")).ok()?;
    let level = capacity.trim().parse::<u8>().ok()?;
    if level > 100 {
        tracing::warn!(
            supply = %path.display(),
            level,
            "implausible sysfs capacity; ignoring supply"
        );
        return None;
    }
    let status = fs::read_to_string(path.join("status")).unwrap_or_default();
    Some(BatteryState {
        level,
        charging: status_is_charging(&status),
        reported_at: SystemTime::now(),
    })
}

/// Maps a sysfs `status` value onto the `isCharging` wire boolean.
///
/// Kernel statuses are `Unknown`, `Charging`, `Discharging`, `Not charging`
/// and `Full`; vendor kernels occasionally emit `Fast charging` variants.
/// Only actively-drawing states count as charging.
fn status_is_charging(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "charging" | "full" | "fast charging" | "trickle charging"
    )
}

/// Process-wide cache of the most recent [`BatteryState`] per device id.
///
/// Cheap to clone; intended to be handed to every [`BatteryPlugin`] built via
/// [`BatteryPlugin::for_device`] so daemon surfaces can query any paired
/// phone's charge without holding its plugin instance.
#[derive(Debug, Clone, Default)]
pub struct BatteryCache {
    states: Arc<Mutex<std::collections::HashMap<String, BatteryState>>>,
}

impl BatteryCache {
    /// Creates an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `state` for `device_id`.
    pub fn set(&self, device_id: &str, state: BatteryState) {
        if let Ok(mut states) = self.states.lock() {
            states.insert(device_id.to_owned(), state);
        }
    }

    /// Returns the most recent state recorded for `device_id`, if any.
    #[must_use]
    pub fn get(&self, device_id: &str) -> Option<BatteryState> {
        let states = self.states.lock().ok()?;
        states.get(device_id).copied()
    }
}

/// Answers battery requests from the phone and stores reports from it.
///
/// One instance serves one connection (the supervisor builds it per device);
/// bind it to a [`BatteryCache`] with [`BatteryPlugin::for_device`] when
/// cross-device visibility is wanted.
#[derive(Debug)]
pub struct BatteryPlugin {
    last: Option<BatteryState>,
    cache: Option<BatteryCache>,
    device_id: Option<String>,
    reader: fn() -> Option<BatteryState>,
}

impl BatteryPlugin {
    /// Create a plugin instance with no remote report observed yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last: None,
            cache: None,
            device_id: None,
            reader: read_local_battery,
        }
    }

    /// Bind the instance to `device_id`, mirroring every received report into
    /// `cache` for process-wide lookup.
    #[must_use]
    pub fn for_device(cache: BatteryCache, device_id: impl Into<String>) -> Self {
        Self {
            last: None,
            cache: Some(cache),
            device_id: Some(device_id.into()),
            reader: read_local_battery,
        }
    }

    /// Overrides the local supply probe. Dependency-injection seam for tests
    /// and exotic platforms; production instances always use
    /// [`read_local_battery`].
    #[must_use]
    pub fn with_reader(mut self, reader: fn() -> Option<BatteryState>) -> Self {
        self.reader = reader;
        self
    }

    /// Most recent report pushed by the phone, if any.
    #[must_use]
    pub fn last_state(&self) -> Option<BatteryState> {
        self.last
    }

    /// Validates and records an inbound `kdeconnect.battery` body.
    ///
    /// Bodies without an integer `currentCharge` in `0..=100` range semantics
    /// (negative and fractional values included) are dropped with a warning
    /// rather than stored.
    fn record_remote(&mut self, pkt: &Packet) {
        let Some(raw_level) = pkt.body.get("currentCharge").and_then(Value::as_u64) else {
            tracing::warn!(
                plugin = meta::BATTERY.name,
                "battery report without numeric currentCharge; ignoring"
            );
            return;
        };
        let state = BatteryState {
            level: u8::try_from(raw_level.min(100)).unwrap_or(FALLBACK_LEVEL),
            charging: pkt
                .body
                .get("isCharging")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            reported_at: SystemTime::now(),
        };
        tracing::debug!(
            plugin = meta::BATTERY.name,
            level = state.level,
            charging = state.charging,
            "stored remote battery report"
        );
        if let Some((cache, device_id)) = self.cache.as_ref().zip(self.device_id.as_deref()) {
            cache.set(device_id, state);
        }
        self.last = Some(state);
    }
}

impl Default for BatteryPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for BatteryPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &meta::BATTERY
    }

    fn handle(&mut self, pkt: &Packet) -> Vec<Packet> {
        match pkt.ty() {
            TYPE_BATTERY_REQUEST => {
                let state = (self.reader)().unwrap_or_else(fallback_state);
                tracing::debug!(
                    plugin = meta::BATTERY.name,
                    level = state.level,
                    charging = state.charging,
                    "answering battery request"
                );
                vec![build_report(state.level, state.charging)]
            }
            TYPE_BATTERY => {
                self.record_remote(pkt);
                Vec::new()
            }
            other => {
                tracing::debug!(
                    plugin = meta::BATTERY.name,
                    got = other,
                    "ignoring foreign packet"
                );
                Vec::new()
            }
        }
    }
}

/// Builds [`BatteryPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct BatteryFactory;

impl PluginFactory for BatteryFactory {
    fn meta(&self) -> &'static PluginMeta {
        &meta::BATTERY
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(BatteryPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_protocol::TYPE_PING;
    use serde_json::json;

    /// Hermetic probe standing in for a real sysfs battery.
    fn probe_present() -> Option<BatteryState> {
        Some(BatteryState {
            level: 73,
            charging: true,
            reported_at: SystemTime::now(),
        })
    }

    /// Hermetic probe modelling hosts without readable power supplies.
    fn probe_missing() -> Option<BatteryState> {
        None
    }

    #[test]
    fn build_report_body_matches_upstream_shape() {
        let report = build_report(87, true);
        assert_eq!(report.ty(), TYPE_BATTERY);
        assert_eq!(report.body["currentCharge"], 87);
        assert_eq!(report.body["isCharging"], true);
        assert_eq!(report.body["thresholdEvent"], 0);
    }

    #[test]
    fn build_report_clamps_levels_above_full() {
        assert_eq!(build_report(200, false).body["currentCharge"], 100);
    }

    #[test]
    fn request_replies_with_probed_level_and_charging_flag() {
        let mut plugin = BatteryPlugin::new().with_reader(probe_present);
        let replies = plugin.handle(&Packet::new(TYPE_BATTERY_REQUEST, json!({})));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].ty(), TYPE_BATTERY);
        assert_eq!(replies[0].body["currentCharge"], 73);
        assert_eq!(replies[0].body["isCharging"], true);
    }

    #[test]
    fn request_falls_back_when_no_local_battery_is_readable() {
        let mut plugin = BatteryPlugin::new().with_reader(probe_missing);
        let replies = plugin.handle(&Packet::new(TYPE_BATTERY_REQUEST, json!({})));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].body["currentCharge"], FALLBACK_LEVEL);
        assert_eq!(replies[0].body["isCharging"], false);
    }

    #[test]
    fn inbound_report_is_stored_and_exposed() {
        let mut plugin = BatteryPlugin::new();
        assert!(plugin.last_state().is_none());
        let replies = plugin.handle(&Packet::new(
            TYPE_BATTERY,
            json!({"currentCharge": 66, "isCharging": true}),
        ));
        assert!(replies.is_empty());
        let state = plugin.last_state().expect("report must be stored");
        assert_eq!(state.level(), 66);
        assert!(state.charging());
    }

    #[test]
    fn malformed_reports_are_ignored_without_panicking() {
        let bodies = vec![
            Value::Null,
            json!({}),
            json!({"isCharging": true}),
            json!({"currentCharge": null}),
            json!({"currentCharge": -5}),
            json!({"currentCharge": 42.5}),
            json!({"currentCharge": "half"}),
            json!({"currentCharge": [50]}),
            json!({"currentCharge": {"v": 50}}),
            json!("not an object"),
        ];
        let mut plugin = BatteryPlugin::new();
        for body in bodies {
            assert!(plugin.handle(&Packet::new(TYPE_BATTERY, body)).is_empty());
            assert!(plugin.last_state().is_none(), "garbage must not be stored");
        }
    }

    #[test]
    fn cache_keeps_devices_isolated() {
        let cache = BatteryCache::new();
        let mut phone = BatteryPlugin::for_device(cache.clone(), "phone");
        let mut tablet = BatteryPlugin::for_device(cache.clone(), "tablet");
        phone.handle(&Packet::new(
            TYPE_BATTERY,
            json!({"currentCharge": 20, "isCharging": false}),
        ));
        tablet.handle(&Packet::new(
            TYPE_BATTERY,
            json!({"currentCharge": 90, "isCharging": true}),
        ));
        assert_eq!(cache.get("phone").expect("phone state").level(), 20);
        assert_eq!(cache.get("tablet").expect("tablet state").level(), 90);
        assert!(cache.get("laptop").is_none());
    }

    #[test]
    fn foreign_packet_types_change_nothing() {
        let mut plugin = BatteryPlugin::new();
        assert!(plugin.handle(&Packet::new(TYPE_PING, json!({}))).is_empty());
        assert!(plugin
            .handle(&Packet::new("kdeconnect.telephony", json!({})))
            .is_empty());
        assert!(plugin.last_state().is_none());
    }

    #[test]
    fn factory_builds_fresh_instances() {
        assert_eq!(BatteryFactory.meta().name, "battery");
        let mut plugin = BatteryFactory.create();
        assert_eq!(plugin.meta().name, "battery");
        assert!(plugin.handle(&Packet::new(TYPE_PING, json!({}))).is_empty());
    }
}
