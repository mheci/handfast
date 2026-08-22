//! UI-side data model for the Handfast GUI.
//!
//! The IPC protocol fixes the shapes of requests, responses and server
//! events, but a successful response carries free-form JSON in its `result`
//! field (see docs/IPC.md). Every parser below is therefore defensive:
//! fields are matched by several likely keys with sensible defaults, and
//! malformed entries are skipped instead of failing the whole list.

use serde_json::{Map, Value};

/// Connection status of the IPC bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnState {
    /// No connection to the daemon.
    Disconnected,
    /// A connect attempt is currently in flight.
    Connecting,
    /// Connected to the daemon socket.
    Connected,
}

impl ConnState {
    /// Short label for status displays.
    #[must_use]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
        }
    }
}

/// Sidebar tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    /// Known devices plus the selected device's plugins.
    Devices,
    /// File transfer progress rows.
    Transfers,
    /// Notifications mirrored from paired phones.
    Notifications,
    /// Log lines forwarded by the daemon.
    Logs,
}

impl Tab {
    /// All tabs in display order.
    pub(crate) const ALL: [Self; 4] = [
        Self::Devices,
        Self::Transfers,
        Self::Notifications,
        Self::Logs,
    ];

    /// Human-readable tab label.
    #[must_use]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Devices => "Devices",
            Self::Transfers => "Transfers",
            Self::Notifications => "Notifications",
            Self::Logs => "Logs",
        }
    }
}

/// One device row of the Devices tab.
///
/// Wire events only carry `{id, name}` (device found) and `{id, state}`
/// updates; richer metadata arrives through the daemon's device-list response
/// when it chooses to provide it.
#[derive(Debug, Clone)]
pub(crate) struct DeviceCard {
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

impl DeviceCard {
    /// Card synthesized from a bare device-found event; kind/state unknown.
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

    /// Authoritative transition from a state-changed event; a card counts as
    /// paired exactly while the daemon reports the "paired" state.
    pub(crate) fn apply_state(&mut self, state: &str) {
        self.state = state.to_owned();
        self.paired = state == "paired";
    }

    /// Insert-or-replace by id.
    pub(crate) fn upsert(devices: &mut Vec<Self>, card: Self) {
        match devices.iter_mut().find(|device| device.id == card.id) {
            Some(slot) => *slot = card,
            None => devices.push(card),
        }
    }
}

/// One plugin toggle row of the selected device.
#[derive(Debug, Clone)]
pub(crate) struct PluginRow {
    /// Plugin identifier used in requests.
    pub(crate) name: String,
    /// Human-readable title shown next to the checkbox.
    pub(crate) title: String,
    /// Whether the plugin is currently enabled on the device.
    pub(crate) enabled: bool,
}

/// Progress row for one ongoing transfer.
#[derive(Debug, Clone)]
pub(crate) struct TransferRow {
    /// Transfer identifier.
    pub(crate) id: String,
    /// Bytes transferred so far.
    pub(crate) done: u64,
    /// Total transfer size in bytes.
    pub(crate) total: u64,
}

impl TransferRow {
    /// Progress as a percentage clamped to `0.0..=100.0`; zero-total
    /// transfers read as 0% so the bar stays well-defined.
    #[must_use]
    pub(crate) fn percent(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        let done = self.done.min(self.total);
        (done as f32 / self.total as f32) * 100.0
    }

    /// Insert-or-replace by id.
    pub(crate) fn upsert(transfers: &mut Vec<Self>, row: Self) {
        match transfers.iter_mut().find(|transfer| transfer.id == row.id) {
            Some(slot) => *slot = row,
            None => transfers.push(row),
        }
    }
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
fn field_str(obj: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_str).map(str::to_owned))
}

/// First present boolean field among `keys`.
fn field_bool(obj: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_bool))
}

/// Accept either a bare array or an object wrapping one under `wrapper_key`.
fn list_items<'a>(value: &'a Value, wrapper_key: &str) -> Option<&'a [Value]> {
    match value.as_array() {
        Some(items) => Some(items.as_slice()),
        None => value.get(wrapper_key).and_then(Value::as_array),
    }
}

/// Parse a device-list result payload.
#[must_use]
pub(crate) fn parse_devices(value: &Value) -> Vec<DeviceCard> {
    let Some(items) = list_items(value, "devices") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let id = field_str(obj, &["id", "device_id"])?;
            let name = field_str(obj, &["name", "device_name"]).unwrap_or_else(|| id.clone());
            let kind = field_str(obj, &["type", "kind", "device_type"])
                .unwrap_or_else(|| "unknown".to_owned());
            let state = field_str(obj, &["state", "status"]).unwrap_or_else(|| "found".to_owned());
            let paired = field_bool(obj, &["paired"]).unwrap_or(state == "paired");
            Some(DeviceCard {
                id,
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
            let title = field_str(obj, &["title", "label"]).unwrap_or_else(|| name.clone());
            let enabled = field_bool(obj, &["enabled", "active"]).unwrap_or(false);
            Some(PluginRow {
                name,
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
                id: field_str(obj, &["id", "notification_id"])?,
                app: field_str(obj, &["app", "application", "source"]).unwrap_or_default(),
                title: field_str(obj, &["title", "summary"]).unwrap_or_default(),
                body: field_str(obj, &["body", "text", "message"]).unwrap_or_default(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_devices_accepts_bare_array() {
        let value = json!([
            {"id": "a", "name": "Pixel", "type": "phone", "paired": true},
            {"id": "b", "name": "Laptop"}
        ]);
        let cards = parse_devices(&value);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].kind, "phone");
        assert!(cards[0].paired);
        assert_eq!(cards[1].kind, "unknown");
        assert_eq!(cards[1].state, "found");
        assert!(!cards[1].paired);
    }

    #[test]
    fn parse_devices_accepts_wrapped_object_and_falls_back_to_id() {
        let value = json!({"devices": [{"device_id": "c"}]});
        let cards = parse_devices(&value);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id, "c");
        // Missing name falls back to the id; missing paired falls back to the
        // default "found" state rather than implying a pairing.
        assert_eq!(cards[0].name, "c");
        assert!(!cards[0].paired);
    }

    #[test]
    fn parse_devices_skips_non_objects_and_rejects_non_arrays() {
        assert!(parse_devices(&json!("nope")).is_empty());
        assert!(parse_devices(&json!(42)).is_empty());
        assert!(parse_devices(&json!([1, "x", null])).is_empty());
    }

    #[test]
    fn parse_devices_treats_state_as_paired_hint() {
        let value = json!([{"id": "d", "state": "connected"}]);
        let cards = parse_devices(&value);
        assert_eq!(cards[0].state, "connected");
        assert!(!cards[0].paired);
        let value = json!([{"id": "d", "status": "paired"}]);
        let cards = parse_devices(&value);
        assert!(cards[0].paired);
    }

    #[test]
    fn parse_plugins_defaults_title_and_enabled() {
        let value = json!({"plugins": [
            {"name": "ping"},
            {"plugin": "sms", "label": "SMS", "active": true}
        ]});
        let plugins = parse_plugins(&value);
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].title, "ping");
        assert!(!plugins[0].enabled);
        assert_eq!(plugins[1].title, "SMS");
        assert!(plugins[1].enabled);
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
    }

    #[test]
    fn transfer_percent_clamps() {
        let row = TransferRow {
            id: "t".into(),
            done: 50,
            total: 100,
        };
        assert_eq!(row.percent(), 50.0);
        let zero = TransferRow {
            id: "t".into(),
            done: 10,
            total: 0,
        };
        assert_eq!(zero.percent(), 0.0);
        let over = TransferRow {
            id: "t".into(),
            done: 150,
            total: 100,
        };
        assert_eq!(over.percent(), 100.0);
    }

    #[test]
    fn upsert_replaces_matching_ids() {
        let mut cards = vec![DeviceCard::from_found("a", "A")];
        DeviceCard::upsert(&mut cards, DeviceCard::from_found("b", "B"));
        assert_eq!(cards.len(), 2);
        DeviceCard::upsert(&mut cards, DeviceCard::from_found("a", "A2"));
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].name, "A2");

        let mut rows = Vec::new();
        TransferRow::upsert(
            &mut rows,
            TransferRow {
                id: "t".into(),
                done: 1,
                total: 2,
            },
        );
        TransferRow::upsert(
            &mut rows,
            TransferRow {
                id: "t".into(),
                done: 3,
                total: 4,
            },
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].done, 3);
    }

    #[test]
    fn apply_state_tracks_pairing_exactly() {
        let mut card = DeviceCard::from_found("a", "A");
        card.apply_state("paired");
        assert!(card.paired);
        card.apply_state("reachable");
        assert!(!card.paired);
        assert_eq!(card.state, "reachable");
    }
}
