//! UI-side data model shared by the one-shot commands and the interactive
//! interface.
//!
//! Successful daemon replies carry free-form JSON (`Response::Ok.result`), so
//! every parser here is defensive: fields are probed under several likely keys
//! with sensible defaults, and malformed entries are skipped instead of
//! failing the whole list. This mirrors the approach taken by the GUI crate.

use serde_json::{Map, Value};

/// One device row of the Devices tab / `devices` table.
#[derive(Debug, Clone)]
pub(crate) struct DeviceEntry {
    /// Stable device identifier.
    pub(crate) id: String,
    /// Human-readable device name.
    pub(crate) name: String,
    /// Coarse device kind if the daemon reported one.
    pub(crate) kind: String,
    /// Whether an active pairing exists.
    pub(crate) paired: bool,
    /// Latest connectivity state label.
    pub(crate) state: String,
}

impl DeviceEntry {
    /// Entry synthesized from a bare device-found event; kind/state unknown.
    #[must_use]
    pub(crate) fn from_found(id: &str, name: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: "unknown".to_owned(),
            paired: false,
            state: "found".to_owned(),
        }
    }

    /// Authoritative transition from a state-changed event; an entry counts as
    /// paired exactly while the daemon reports the "paired" state.
    pub(crate) fn apply_state(&mut self, state: &str) {
        self.state = state.to_owned();
        self.paired = state == "paired";
    }

    /// Insert-or-replace by id.
    pub(crate) fn upsert(devices: &mut Vec<Self>, entry: Self) {
        match devices.iter_mut().find(|device| device.id == entry.id) {
            Some(slot) => *slot = entry,
            None => devices.push(entry),
        }
    }
}

/// One plugin toggle row of the selected device's detail panel.
#[derive(Debug, Clone)]
pub(crate) struct PluginRow {
    /// Plugin identifier used in requests.
    pub(crate) name: String,
    /// Human-readable title shown next to the checkbox.
    pub(crate) title: String,
    /// Whether the plugin is currently enabled on the device.
    pub(crate) enabled: bool,
}

/// One mirrored notification row.
#[derive(Debug, Clone)]
pub(crate) struct NotifRow {
    /// Notification identifier used when dismissing.
    pub(crate) id: String,
    /// Originating application name.
    pub(crate) app: String,
    /// Notification title.
    pub(crate) title: String,
    /// Notification body text.
    pub(crate) body: String,
}

/// First present string field among `keys`.
pub(crate) fn field_str<'a>(obj: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_str))
}

/// First present boolean field among `keys`.
pub(crate) fn field_bool(obj: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_bool))
}

/// First present integer field among `keys`.
pub(crate) fn field_u64(obj: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_u64))
}

/// Accept either a bare array or an object wrapping one under `wrapper_key`.
pub(crate) fn list_items<'a>(value: &'a Value, wrapper_key: &str) -> Option<&'a [Value]> {
    match value.as_array() {
        Some(items) => Some(items.as_slice()),
        None => value.get(wrapper_key).and_then(Value::as_array),
    }
}

/// Parse a device-list result payload.
#[must_use]
pub(crate) fn parse_devices(value: &Value) -> Vec<DeviceEntry> {
    let Some(items) = list_items(value, "devices") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let id = field_str(obj, &["id", "device_id"])?;
            let name = field_str(obj, &["name", "device_name"])
                .unwrap_or(id)
                .to_owned();
            let kind = field_str(obj, &["type", "kind", "device_type"])
                .unwrap_or("unknown")
                .to_owned();
            let state = field_str(obj, &["state", "status"])
                .unwrap_or("found")
                .to_owned();
            let paired = field_bool(obj, &["paired"]).unwrap_or(state == "paired");
            Some(DeviceEntry {
                id: id.to_owned(),
                name,
                kind,
                paired,
                state,
            })
        })
        .collect()
}

/// Parse a plugin-list result payload for one device.
#[must_use]
pub(crate) fn parse_plugins(value: &Value) -> Vec<PluginRow> {
    let Some(items) = list_items(value, "plugins") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let name = field_str(obj, &["name", "plugin", "id"])?;
            let title = field_str(obj, &["title", "label"])
                .unwrap_or(name)
                .to_owned();
            let enabled = field_bool(obj, &["enabled", "active"]).unwrap_or(false);
            Some(PluginRow {
                name: name.to_owned(),
                title,
                enabled,
            })
        })
        .collect()
}

/// Parse a notification-list result payload.
#[must_use]
pub(crate) fn parse_notifications(value: &Value) -> Vec<NotifRow> {
    let Some(items) = list_items(value, "notifications") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            Some(NotifRow {
                id: field_str(obj, &["id", "notification_id"])?.to_owned(),
                app: field_str(obj, &["app", "application", "source"])
                    .unwrap_or("")
                    .to_owned(),
                title: field_str(obj, &["title", "summary"])
                    .unwrap_or("")
                    .to_owned(),
                body: field_str(obj, &["body", "text", "message"])
                    .unwrap_or("")
                    .to_owned(),
            })
        })
        .collect()
}

/// Pull human-readable clipboard text out of a clipboard result payload,
/// accepting a bare string or `{"text": …}`.
#[must_use]
pub(crate) fn extract_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        other => other.to_string(),
    }
}

/// Short display form of a long identifier (first 8 characters).
#[must_use]
pub(crate) fn short_hash(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Render bytes with binary units (`976 B`, `1.5 KiB`, `5.0 MiB`).
#[must_use]
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = f64::from(bytes);
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `"yes"`/`"no"` for boolean table cells.
#[must_use]
pub(crate) fn yes_no(flag: bool) -> &'static str {
    if flag {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_devices_accepts_bare_array() {
        let value = json!([
            {"id": "a", "name": "Pixel", "type": "phone", "paired": true},
            {"b": true}
        ]);
        let entries = parse_devices(&value);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "a");
        assert_eq!(entries[0].kind, "phone");
        assert!(entries[0].paired);
    }

    #[test]
    fn parse_devices_accepts_wrapped_object_and_falls_back_to_id() {
        let value = json!({"devices": [{"device_id": "c"}]});
        let entries = parse_devices(&value);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "c");
        assert_eq!(entries[0].kind, "unknown");
        assert_eq!(entries[0].state, "found");
        assert!(!entries[0].paired);
    }

    #[test]
    fn parse_devices_treats_state_paired_as_pairing_hint() {
        let value = json!([{"id": "d", "status": "paired"}]);
        let entries = parse_devices(&value);
        assert!(entries[0].paired);
        let value = json!([{"id": "d", "state": "reachable"}]);
        let entries = parse_devices(&value);
        assert!(!entries[0].paired);
        assert_eq!(entries[0].state, "reachable");
    }

    #[test]
    fn parse_plugins_defaults_title_and_enabled() {
        let value = json!({"plugins": [
            {"name": "ping"},
            {"plugin": "sms", "label": "SMS", "active": true}
        ]});
        let rows = parse_plugins(&value);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "ping");
        assert!(!rows[0].enabled);
        assert_eq!(rows[1].name, "sms");
        assert_eq!(rows[1].title, "SMS");
        assert!(rows[1].enabled);
    }

    #[test]
    fn parse_notifications_requires_only_an_id() {
        let value = json!([
            {"notification_id": "n1", "app": "kmail", "title": "Hi", "body": "Yo"},
            {"id": "n2"}
        ]);
        let rows = parse_notifications(&value);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].app, "kmail");
        assert_eq!(rows[1].title, "");
        assert_eq!(rows[1].body, "");
    }

    #[test]
    fn parsers_reject_non_arrays_and_non_objects() {
        assert!(parse_devices(&json!("nope")).is_empty());
        assert!(parse_devices(&json!(42)).is_empty());
        assert!(parse_devices(&json!([1, "x", null])).is_empty());
        assert!(parse_plugins(&json!([[]])).is_empty());
        assert!(parse_notifications(&json!(null)).is_empty());
    }

    #[test]
    fn extract_text_handles_all_shapes() {
        assert_eq!(extract_text(&json!("plain")), "plain");
        assert_eq!(extract_text(&json!({"text": "wrapped"})), "wrapped");
        assert_eq!(extract_text(&json!({"text": null})), "");
        assert_eq!(extract_text(&json!(7)), "7");
    }

    #[test]
    fn upsert_replaces_matching_ids_only() {
        let mut devices = vec![DeviceEntry::from_found("a", "A")];
        DeviceEntry::upsert(&mut devices, DeviceEntry::from_found("b", "B"));
        assert_eq!(devices.len(), 2);
        DeviceEntry::upsert(&mut devices, DeviceEntry::from_found("a", "A2"));
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, "A2");
    }

    #[test]
    fn apply_state_tracks_pairing_exactly() {
        let mut card = DeviceEntry::from_found("a", "A");
        card.apply_state("paired");
        assert!(card.paired);
        card.apply_state("reachable");
        assert!(!card.paired);
        assert_eq!(card.state, "reachable");
    }

    #[test]
    fn short_hash_truncates_but_keeps_short_ids() {
        assert_eq!(short_hash("abcdefghijklmnop"), "abcdefgh");
        assert_eq!(short_hash("abc"), "abc");
        assert_eq!(short_hash(""), "");
    }

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(976), "976 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn yes_no_is_stable() {
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
    }
}
