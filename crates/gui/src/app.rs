//! Application state, message type and reducer for the Handfast GUI.
//!
//! The reducer is deliberately total: every [`Message`] maps to plain state
//! transitions returning [`Task::none`]. All side effects (IPC requests) go
//! through the bridge command channel owned by the subscription driver, so
//! `update` stays pure enough to drive directly from unit tests without any
//! windowing or async runtime.

use std::collections::VecDeque;

use handfast_ipc::{ServerEvent, IPC_VERSION};
use iced::futures::channel::mpsc;
use iced::{Subscription, Task};

use crate::bridge::{self, BridgeIn};
use crate::model::{
    apply_persisted_toggles, load_plugin_toggles, save_plugin_toggles, split_toggle_key,
    toggle_key, ConnState, DeviceCard, NotifRow, PluginRow, PluginToggles, Tab, TransferRow,
};

/// Maximum number of retained log lines.
const LOG_CAP: usize = 300;

/// Maximum number of retained notifications (oldest evicted first).
const NOTIF_CAP: usize = 100;

/// Cloneable handle to the IPC driver's command channel.
#[derive(Clone)]
pub(crate) struct Bridge(mpsc::UnboundedSender<BridgeIn>);

impl Bridge {
    /// Wrap the driver-side command sender; construction stays inside the
    /// crate so the channel field can remain private.
    pub(crate) fn new(sender: mpsc::UnboundedSender<BridgeIn>) -> Self {
        Self(sender)
    }
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bridge(..)")
    }
}

/// All inputs the GUI reacts to.
#[derive(Debug, Clone)]
pub(crate) enum Message {
    /// Explicit no-op; reserved for closures over widget callbacks.
    #[allow(dead_code)]
    Nothing,
    /// Sidebar tab switched.
    TabSelected(Tab),
    /// Error banner dismissed.
    BannerClosed,
    /// Bridge connection status changed.
    IpcStatus(ConnState),
    /// The IPC driver announced its command channel.
    BridgeReady(Bridge),
    /// The daemon socket was lost; text explains why.
    ConnectionLost(String),
    /// A data load failed; text explains why.
    LoadFailed(String),
    /// Full device list replaced.
    DevicesLoaded(Vec<DeviceCard>),
    /// Plugin rows replaced (for the selected device).
    PluginsLoaded(Vec<PluginRow>),
    /// Notification snapshot replaced.
    NotificationsLoaded(Vec<NotifRow>),
    /// Acknowledgement of a pair request.
    Paired(Result<(), String>),
    /// Acknowledgement of an unpair request.
    Unpaired(Result<(), String>),
    /// Acknowledgement of a send-file request.
    FileQueued(Result<(), String>),
    /// Acknowledgement of a notification dismissal.
    Dismissed(Result<(), String>),
    /// Acknowledgement of a plugin enable/disable request.
    PluginSet(Result<(), String>),
    /// Acknowledgement of a clipboard write.
    ClipboardSet(Result<(), String>),
    /// Fan-in of all server-pushed events.
    Event(ServerEvent),
    /// A device card was clicked.
    DeviceSelected(usize),
    /// Pair button pressed for a device id.
    PairPressed(String),
    /// Unpair button pressed for a device id.
    UnpairPressed(String),
    /// Plugin checkbox toggled (name, desired state).
    TogglePlugin(String, bool),
    /// Settings gear pressed; flips the collapsible settings panel.
    SettingsPressed,
    /// A device-scoped checkbox in the settings panel flipped
    /// (`"device_id:plugin_name"` key, desired state).
    ToggleSavedPlugin(String, bool),
    /// Queue-file button pressed.
    QueueFilePressed,
    /// File-path input changed.
    PathChanged(String),
    /// Clipboard input changed.
    ClipboardChanged(String),
    /// Clipboard send button or input submit pressed.
    SendPressed,
    /// Manual device-list refresh requested.
    RefreshPressed,
    /// Dismiss button pressed for a notification id.
    DismissPressed(String),
}

/// Root application state.
pub(crate) struct HandfastApp {
    /// Current bridge connection status.
    pub(crate) conn: ConnState,
    /// Active sidebar tab.
    pub(crate) tab: Tab,
    /// Known devices, in discovery order.
    pub(crate) devices: Vec<DeviceCard>,
    /// Index of the selected device (kept in sync with `selected_id`).
    pub(crate) selected: Option<usize>,
    /// Stable identity of the selected device; survives list reordering.
    selected_id: Option<String>,
    /// Plugins of the selected device.
    pub(crate) plugins: Vec<PluginRow>,
    /// Ongoing transfers keyed by transfer id.
    pub(crate) transfers: Vec<TransferRow>,
    /// Mirrored notifications, newest first.
    pub(crate) notifications: Vec<NotifRow>,
    /// Bounded log lines, oldest first.
    pub(crate) logs: VecDeque<String>,
    /// Top banner text when something needs attention.
    pub(crate) banner: Option<String>,
    /// Command channel into the IPC driver once announced.
    bridge: Option<Bridge>,
    /// Device awaiting a pair acknowledgement.
    pair_target: Option<String>,
    /// Device awaiting an unpair acknowledgement.
    unpair_target: Option<String>,
    /// Draft clipboard text to send.
    pub(crate) clipboard_draft: String,
    /// Last clipboard-send outcome line.
    pub(crate) clipboard_status: Option<String>,
    /// Draft absolute file path to queue for transfer.
    pub(crate) file_path_draft: String,
    /// Wire protocol version reported by the daemon's Hello.
    pub(crate) daemon_version: Option<String>,
    /// Whether the collapsible settings panel is expanded.
    pub(crate) settings_open: bool,
    /// Persisted plugin-enable flags keyed by `"device_id:plugin_name"`.
    pub(crate) plugin_toggles: PluginToggles,
}

impl HandfastApp {
    /// Initial UI state; the bridge connects itself via the subscription.
    #[must_use]
    pub(crate) fn new() -> Self {
        let mut logs = VecDeque::new();
        logs.push_back("handfast-gui started".to_owned());
        Self {
            conn: ConnState::Disconnected,
            tab: Tab::Devices,
            devices: Vec::new(),
            selected: None,
            selected_id: None,
            plugins: Vec::new(),
            transfers: Vec::new(),
            notifications: Vec::new(),
            logs,
            banner: None,
            bridge: None,
            pair_target: None,
            unpair_target: None,
            clipboard_draft: String::new(),
            clipboard_status: None,
            file_path_draft: String::new(),
            daemon_version: None,
            settings_open: false,
            plugin_toggles: load_plugin_toggles(),
        }
    }

    /// Long-lived IPC event stream subscription.
    pub(crate) fn subscription(_app: &Self) -> Subscription<Message> {
        bridge::subscription()
    }

    /// The Elm-style reducer; also the entry point used by unit tests.
    pub(crate) fn update(app: &mut Self, message: Message) -> Task<Message> {
        match message {
            Message::Nothing => {}
            Message::TabSelected(tab) => app.tab = tab,
            Message::BannerClosed => app.banner = None,
            Message::IpcStatus(state) => {
                app.conn = state;
                // A fresh successful connect resolves any stale outage text.
                if state == ConnState::Connected {
                    app.banner = None;
                }
            }
            Message::BridgeReady(bridge_handle) => app.bridge = Some(bridge_handle),
            Message::ConnectionLost(text) => app.set_banner(text),
            Message::LoadFailed(text) => app.set_banner(format!("load failed: {text}")),
            Message::DevicesLoaded(cards) => {
                let count = cards.len();
                app.devices = cards;
                // The list was replaced wholesale; old indices mean nothing.
                app.selected = None;
                app.selected_id = None;
                app.plugins.clear();
                app.push_log(format!("loaded {count} device(s)"));
            }
            Message::PluginsLoaded(rows) => {
                let count = rows.len();
                app.plugins = rows;
                // Re-apply persisted device-scoped toggles over the fresh
                // snapshot so restarts and reselections keep manual choices.
                if let Some(device_id) = app.selected_device_id() {
                    apply_persisted_toggles(&mut app.plugins, &device_id, &app.plugin_toggles);
                }
                app.push_log(format!("loaded {count} plugin(s)"));
            }
            Message::NotificationsLoaded(rows) => app.notifications = rows,
            Message::Paired(result) => finish_pairing(app, result, false),
            Message::Unpaired(result) => finish_pairing(app, result, true),
            Message::FileQueued(result) => match result {
                Ok(()) => app.push_log("file queued for transfer"),
                Err(err) => app.set_banner(format!("send failed: {err}")),
            },
            Message::Dismissed(result) => match result {
                Ok(()) => app.push_log("notification dismissed"),
                Err(err) => app.set_banner(format!("dismiss failed: {err}")),
            },
            Message::PluginSet(result) => {
                if let Err(err) = result {
                    app.set_banner(format!("plugin change failed: {err}"));
                }
            }
            Message::ClipboardSet(result) => match result {
                Ok(()) => {
                    app.clipboard_status = Some("copied to daemon".to_owned());
                    app.push_log("clipboard sent");
                }
                Err(err) => {
                    app.clipboard_status = Some(format!("error: {err}"));
                    app.push_log(format!("[error] clipboard send failed: {err}"));
                }
            },
            Message::Event(event) => handle_event(app, event),
            Message::DeviceSelected(index) => {
                if let Some(device) = app.devices.get(index) {
                    app.selected_id = Some(device.id.clone());
                    app.selected = Some(index);
                    app.plugins.clear();
                    let id = device.id.clone();
                    if !app.dispatch(BridgeIn::ListPlugins(id)) {
                        app.set_banner("not connected");
                    }
                }
            }
            Message::PairPressed(id) => {
                if let Some(card) = app.device_mut(&id) {
                    card.state = "pairing".to_owned();
                }
                app.pair_target = Some(id.clone());
                if !app.dispatch(BridgeIn::Pair(id)) {
                    app.set_banner("not connected");
                }
            }
            Message::UnpairPressed(id) => {
                if let Some(card) = app.device_mut(&id) {
                    card.state = "unpairing".to_owned();
                }
                app.unpair_target = Some(id.clone());
                if !app.dispatch(BridgeIn::Unpair(id)) {
                    app.set_banner("not connected");
                }
            }
            Message::TogglePlugin(name, enabled) => {
                // Optimistic flip; an authoritative refresh follows shortly.
                if let Some(row) = app.plugins.iter_mut().find(|row| row.name == name) {
                    row.enabled = enabled;
                }
                if let Some(device_id) = app.selected_device_id() {
                    record_plugin_toggle(app, &device_id, &name, enabled);
                    if !app.dispatch(BridgeIn::SetPlugin {
                        device_id,
                        plugin: name,
                        enabled,
                    }) {
                        app.set_banner("not connected");
                    }
                }
            }
            Message::SettingsPressed => app.settings_open = !app.settings_open,
            Message::ToggleSavedPlugin(key, enabled) => match split_toggle_key(&key) {
                Some((device_id, plugin)) => {
                    record_plugin_toggle(app, device_id, plugin, enabled);
                    if !app.dispatch(BridgeIn::SetPlugin {
                        device_id: device_id.to_owned(),
                        plugin: plugin.to_owned(),
                        enabled,
                    }) {
                        app.set_banner("not connected");
                    }
                }
                None => app.set_banner(format!("malformed plugin toggle key: {key}")),
            },
            Message::QueueFilePressed => {
                let Some(device_id) = app.selected_device_id() else {
                    app.set_banner("select a device first");
                    return Task::none();
                };
                let path = app.file_path_draft.trim().to_owned();
                if path.is_empty() {
                    app.set_banner("enter an absolute file path first");
                } else if !app.dispatch(BridgeIn::SendFile { device_id, path }) {
                    app.set_banner("not connected");
                }
            }
            Message::DismissPressed(id) => {
                app.notifications.retain(|row| row.id != id);
                let _ = app.dispatch(BridgeIn::DismissNotification(id));
            }
            Message::PathChanged(path) => app.file_path_draft = path,
            Message::ClipboardChanged(text) => app.clipboard_draft = text,
            Message::SendPressed => {
                let text = app.clipboard_draft.trim().to_owned();
                if text.is_empty() {
                    app.clipboard_status = Some("nothing to send".to_owned());
                } else {
                    app.clipboard_status = Some("sending".to_owned());
                    if !app.dispatch(BridgeIn::ClipboardSet(text)) {
                        app.clipboard_status = Some("not connected".to_owned());
                    }
                }
            }
            Message::RefreshPressed => {
                if !app.dispatch(BridgeIn::RefreshDevices) {
                    app.set_banner("not connected");
                }
            }
        }
        Task::none()
    }

    /// Record a user-facing error: banner plus a matching log line.
    fn set_banner(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.push_log(format!("[error] {text}"));
        self.banner = Some(text);
    }

    /// Append a line to the bounded log buffer.
    fn push_log(&mut self, line: impl Into<String>) {
        if self.logs.len() >= LOG_CAP {
            self.logs.pop_front();
        }
        self.logs.push_back(line.into());
    }

    /// Mutable lookup by device id.
    fn device_mut(&mut self, id: &str) -> Option<&mut DeviceCard> {
        self.devices.iter_mut().find(|device| device.id == id)
    }

    /// Id of the currently selected device, if still present.
    fn selected_device_id(&self) -> Option<String> {
        self.selected
            .and_then(|index| self.devices.get(index))
            .map(|device| device.id.clone())
    }

    /// Recompute the selection index from its stable id after mutations.
    fn sync_selection(&mut self) {
        self.selected = match &self.selected_id {
            Some(id) => self.devices.iter().position(|device| device.id == *id),
            None => None,
        };
    }

    /// Best-effort command dispatch; reports false when offline.
    fn dispatch(&self, command: BridgeIn) -> bool {
        match &self.bridge {
            Some(bridge) => bridge.0.unbounded_send(command).is_ok(),
            None => false,
        }
    }
}

impl Default for HandfastApp {
    fn default() -> Self {
        Self::new()
    }
}

/// Record and flush one device-scoped plugin toggle; persistence failures
/// degrade to a log line so toggling stays usable offline.
fn record_plugin_toggle(app: &mut HandfastApp, device_id: &str, plugin: &str, enabled: bool) {
    app.plugin_toggles
        .insert(toggle_key(device_id, plugin), enabled);
    if let Err(err) = save_plugin_toggles(&app.plugin_toggles) {
        app.push_log(format!("[warn] persisting plugin toggles failed: {err}"));
    }
}

/// Apply a pair/unpair acknowledgement to the pending target device.
fn finish_pairing(app: &mut HandfastApp, result: Result<(), String>, unpair: bool) {
    let target = if unpair {
        app.unpair_target.take()
    } else {
        app.pair_target.take()
    };
    match result {
        Ok(()) => {
            let Some(id) = target else { return };
            if let Some(card) = app.device_mut(&id) {
                if unpair {
                    card.paired = false;
                    card.state = "found".to_owned();
                } else {
                    card.paired = true;
                    card.state = "paired".to_owned();
                }
            }
            if unpair {
                app.push_log(format!("unpaired {id}"));
            } else {
                app.push_log(format!("paired with {id}"));
            }
        }
        Err(err) => {
            if let Some(id) = &target {
                // Revert the optimistic in-flight label.
                if let Some(card) = app.device_mut(id) {
                    if unpair && card.state == "unpairing" {
                        card.state = "paired".to_owned();
                    } else if !unpair && card.state == "pairing" {
                        card.state = "found".to_owned();
                    }
                }
            }
            let action = if unpair { "unpairing" } else { "pairing" };
            app.set_banner(format!("{action} failed: {err}"));
        }
    }
}

/// Fan-in handler for every server-pushed event.
fn handle_event(app: &mut HandfastApp, event: ServerEvent) {
    match event {
        ServerEvent::Hello {
            version,
            app: name,
            pid,
        } => {
            app.daemon_version = Some(version.to_string());
            app.push_log(format!("hello: {name} pid {pid} (ipc v{version})"));
            if version != IPC_VERSION {
                app.push_log(format!(
                    "[warn] protocol mismatch: gui speaks v{IPC_VERSION}, daemon offers v{version}"
                ));
            }
        }
        ServerEvent::DeviceFound { id, name } => {
            // Re-announcement: upsert keeps richer paired/online state while
            // refreshing the advertised name.
            let mut card = app
                .devices
                .iter()
                .find(|device| device.id == id)
                .cloned()
                .unwrap_or_else(|| DeviceCard::from_found(&id, &name));
            card.name = name;
            DeviceCard::upsert(&mut app.devices, card);
        }
        ServerEvent::DeviceLost { id } => {
            if let Some(index) = app.devices.iter().position(|device| device.id == id) {
                app.devices.remove(index);
            }
            if app.selected_id.as_deref() == Some(id.as_str()) {
                // The selected device itself disappeared.
                app.selected_id = None;
                app.plugins.clear();
            }
            app.sync_selection();
            if app.pair_target.as_deref() == Some(id.as_str()) {
                app.pair_target = None;
            }
            if app.unpair_target.as_deref() == Some(id.as_str()) {
                app.unpair_target = None;
            }
            app.push_log(format!("device lost: {id}"));
        }
        ServerEvent::DeviceStateChanged { id, state } => {
            if let Some(card) = app.device_mut(&id) {
                card.apply_state(&state);
            }
        }
        ServerEvent::TransferAdded {
            id,
            direction,
            file_name,
            total,
            ..
        } => {
            TransferRow::upsert(
                &mut app.transfers,
                TransferRow {
                    id,
                    name: file_name.clone(),
                    done: 0,
                    total,
                },
            );
            app.push_log(format!("transfer {direction}: {file_name}"));
        }
        ServerEvent::TransferProgress { ref id, .. } => {
            // Progress may arrive without a prior Added frame; synthesize a
            // placeholder row so the bar still appears.
            if !app.transfers.iter().any(|row| row.id == *id) {
                app.transfers.push(TransferRow {
                    id: id.clone(),
                    name: id.clone(),
                    done: 0,
                    total: 0,
                });
            }
            if let Some(row) = app.transfers.iter_mut().find(|row| row.id == *id) {
                row.update(&event);
            }
        }
        ServerEvent::TransferFinished { ref id } => {
            if let Some(row) = app.transfers.iter_mut().find(|row| row.id == *id) {
                row.update(&event);
            }
            app.push_log(format!("transfer finished: {id}"));
        }
        ServerEvent::TransferFailed { id, reason } => {
            app.transfers.retain(|row| row.id != id);
            app.push_log(format!("[error] transfer failed: {reason}"));
        }
        ServerEvent::NotificationReceived {
            id,
            app: source,
            title,
            body,
        } => {
            app.notifications.insert(
                0,
                NotifRow {
                    id,
                    app: source,
                    title,
                    body,
                },
            );
            if app.notifications.len() > NOTIF_CAP {
                app.notifications.pop();
            }
        }
        ServerEvent::ClipboardUpdated { text } => {
            app.push_log(format!("remote clipboard: {}", clamp_line(&text, 80)));
        }
        ServerEvent::LogRecord { level, msg } => {
            app.push_log(format!("[{level}] {msg}"));
        }
        ServerEvent::DaemonShutdown => {
            app.push_log("daemon shut down");
            app.set_banner("daemon shut down; waiting for it to come back");
        }
        // Battery, telephony, volume and command-result events carry no GUI
        // state today.
        _ => {}
    }
}

/// Trim long single-line payloads for the log view.
fn clamp_line(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_ipc::ServerEvent as Ev;

    /// Drive a batch of synthetic messages through the reducer.
    fn reduce(app: &mut HandfastApp, messages: &[Message]) {
        for message in messages {
            let _ = HandfastApp::update(app, message.clone());
        }
    }

    /// Drain everything currently queued on the bridge channel.
    fn drained(receiver: &mut mpsc::UnboundedReceiver<BridgeIn>) -> Vec<BridgeIn> {
        let mut commands = Vec::new();
        while let Ok(command) = receiver.try_recv() {
            commands.push(command);
        }
        commands
    }

    /// App with an attached bridge command channel.
    fn app_with_bridge() -> (HandfastApp, mpsc::UnboundedReceiver<BridgeIn>) {
        let (sender, receiver) = mpsc::unbounded::<BridgeIn>();
        let mut app = HandfastApp::new();
        reduce(&mut app, &[Message::BridgeReady(Bridge(sender))]);
        (app, receiver)
    }

    #[test]
    fn device_found_adds_and_then_merges_cards() {
        let mut app = HandfastApp::new();
        reduce(
            &mut app,
            &[Message::Event(Ev::DeviceFound {
                id: "d1".into(),
                name: "Pixel".into(),
            })],
        );
        assert_eq!(app.devices.len(), 1);
        assert_eq!(app.devices[0].name, "Pixel");
        assert_eq!(app.devices[0].kind, "unknown");

        reduce(
            &mut app,
            &[Message::Event(Ev::DeviceFound {
                id: "d1".into(),
                name: "Pixel 2".into(),
            })],
        );
        assert_eq!(app.devices.len(), 1);
        assert_eq!(app.devices[0].name, "Pixel 2");
    }

    #[test]
    fn transfer_progress_updates_matching_id_only() {
        let mut app = HandfastApp::new();
        reduce(
            &mut app,
            &[
                Message::Event(Ev::TransferProgress {
                    id: "t1".into(),
                    bytes_done: 10,
                    bytes_total: 100,
                }),
                Message::Event(Ev::TransferProgress {
                    id: "t2".into(),
                    bytes_done: 5,
                    bytes_total: 50,
                }),
                Message::Event(Ev::TransferProgress {
                    id: "t2".into(),
                    bytes_done: 25,
                    bytes_total: 50,
                }),
            ],
        );
        assert_eq!(app.transfers.len(), 2);
        let t1 = app
            .transfers
            .iter()
            .find(|transfer| transfer.id == "t1")
            .map(|transfer| transfer.done);
        assert_eq!(t1, Some(10));
        let t2 = app
            .transfers
            .iter()
            .find(|transfer| transfer.id == "t2")
            .map(|transfer| transfer.done);
        assert_eq!(t2, Some(25));
    }

    #[test]
    fn transfer_added_seeds_row_and_finish_snaps_it() {
        let mut app = HandfastApp::new();
        reduce(
            &mut app,
            &[Message::Event(Ev::TransferAdded {
                id: "t9".into(),
                device_id: "d1".into(),
                direction: "incoming".into(),
                file_name: "report.pdf".into(),
                total: 500,
            })],
        );
        assert_eq!(app.transfers.len(), 1);
        assert_eq!(app.transfers[0].name, "report.pdf");
        assert_eq!(app.transfers[0].done, 0);

        reduce(
            &mut app,
            &[Message::Event(Ev::TransferProgress {
                id: "t9".into(),
                bytes_done: 200,
                bytes_total: 500,
            })],
        );
        // A duplicate Added frame must not duplicate rows.
        reduce(
            &mut app,
            &[Message::Event(Ev::TransferAdded {
                id: "t9".into(),
                device_id: "d1".into(),
                direction: "incoming".into(),
                file_name: "report.pdf".into(),
                total: 500,
            })],
        );
        assert_eq!(app.transfers.len(), 1);

        reduce(
            &mut app,
            &[Message::Event(Ev::TransferFinished { id: "t9".into() })],
        );
        assert_eq!(app.transfers[0].done, 500);
        assert_eq!(app.transfers[0].percent(), 100.0);
    }

    #[test]
    fn transfer_failed_drops_the_row() {
        let mut app = HandfastApp::new();
        reduce(
            &mut app,
            &[
                Message::Event(Ev::TransferProgress {
                    id: "t1".into(),
                    bytes_done: 10,
                    bytes_total: 100,
                }),
                Message::Event(Ev::TransferFailed {
                    id: "t1".into(),
                    reason: "peer reset".into(),
                }),
            ],
        );
        assert!(app.transfers.is_empty());
    }

    #[test]
    fn settings_button_flips_panel_visibility() {
        let mut app = HandfastApp::new();
        assert!(!app.settings_open);
        reduce(&mut app, &[Message::SettingsPressed]);
        assert!(app.settings_open);
        reduce(&mut app, &[Message::SettingsPressed]);
        assert!(!app.settings_open);
    }

    #[test]
    fn saved_plugin_toggles_persist_and_dispatch() {
        let (mut app, mut receiver) = app_with_bridge();
        reduce(
            &mut app,
            &[Message::Event(Ev::DeviceFound {
                id: "a".into(),
                name: "A".into(),
            })],
        );

        reduce(
            &mut app,
            &[Message::ToggleSavedPlugin("a:ping".into(), true)],
        );
        assert_eq!(app.plugin_toggles.get("a:ping"), Some(&true));
        assert!(matches!(
            drained(&mut receiver).first(),
            Some(BridgeIn::SetPlugin { device_id, plugin, enabled: true })
                if device_id == "a" && plugin == "ping"
        ));

        // Malformed keys neither persist nor dispatch.
        reduce(
            &mut app,
            &[Message::ToggleSavedPlugin("broken".into(), true)],
        );
        assert!(!app.plugin_toggles.contains_key("broken"));
        assert!(drained(&mut receiver).is_empty());
        assert!(matches!(&app.banner, Some(text) if text.contains("malformed")));
    }

    #[test]
    fn plugins_loaded_overlays_persisted_state() {
        let mut app = HandfastApp::new();
        app.plugin_toggles.insert("a:sms".into(), false);
        reduce(
            &mut app,
            &[Message::Event(Ev::DeviceFound {
                id: "a".into(),
                name: "A".into(),
            })],
        );
        reduce(&mut app, &[Message::DeviceSelected(0)]);
        reduce(
            &mut app,
            &[Message::PluginsLoaded(vec![PluginRow {
                name: "sms".into(),
                title: "SMS".into(),
                enabled: true,
            }])],
        );
        // The daemon said enabled=true but the user persisted enabled=false.
        assert!(!app.plugins[0].enabled);
    }

    #[test]
    fn toggling_a_plugin_records_its_persistence_key() {
        let (mut app, mut receiver) = app_with_bridge();
        reduce(
            &mut app,
            &[
                Message::Event(Ev::DeviceFound {
                    id: "a".into(),
                    name: "A".into(),
                }),
                Message::DeviceSelected(0),
            ],
        );
        let _ = receiver.try_recv(); // consume the ListPlugins command

        reduce(&mut app, &[Message::TogglePlugin("ping".into(), true)]);
        assert_eq!(app.plugin_toggles.get("a:ping"), Some(&true));
        assert!(matches!(
            drained(&mut receiver).first(),
            Some(BridgeIn::SetPlugin { plugin, enabled: true, .. }) if plugin == "ping"
        ));
    }

    #[test]
    fn log_buffer_caps_at_limit() {
        let mut app = HandfastApp::new();
        let mut messages = Vec::new();
        for i in 0..(LOG_CAP + 10) {
            messages.push(Message::Event(Ev::LogRecord {
                level: "info".into(),
                msg: format!("line-{i}"),
            }));
        }
        reduce(&mut app, &messages);
        assert_eq!(app.logs.len(), LOG_CAP);
        // One boot line plus LOG_CAP+10 records: the oldest eleven fell off.
        assert!(app
            .logs
            .back()
            .is_some_and(|line| line.contains("line-309")));
        assert!(app
            .logs
            .front()
            .is_some_and(|line| line.contains("line-10")));
    }

    #[test]
    fn notifications_cap_and_dismiss() {
        let mut app = HandfastApp::new();
        let mut messages = Vec::new();
        for i in 0..(NOTIF_CAP + 5) {
            messages.push(Message::Event(Ev::NotificationReceived {
                id: format!("n{i}"),
                app: "app".into(),
                title: format!("t{i}"),
                body: String::new(),
            }));
        }
        reduce(&mut app, &messages);
        assert_eq!(app.notifications.len(), NOTIF_CAP);
        assert_eq!(app.notifications[0].id, format!("n{}", NOTIF_CAP + 4));

        reduce(
            &mut app,
            &[Message::DismissPressed(format!("n{}", NOTIF_CAP + 4))],
        );
        assert!(app
            .notifications
            .iter()
            .all(|row| row.id != format!("n{}", NOTIF_CAP + 4)));
    }

    #[test]
    fn selecting_a_device_fetches_its_plugins() {
        let (mut app, mut receiver) = app_with_bridge();
        app.plugins.push(PluginRow {
            name: "stale".into(),
            title: "stale".into(),
            enabled: false,
        });
        reduce(
            &mut app,
            &[
                Message::Event(Ev::DeviceFound {
                    id: "a".into(),
                    name: "A".into(),
                }),
                Message::DeviceSelected(0),
            ],
        );
        assert!(matches!(drained(&mut receiver).first(),
                Some(BridgeIn::ListPlugins(id)) if id == "a"));
        assert_eq!(app.selected, Some(0));
        // Stale rows were dropped pending the authoritative refresh.
        assert!(app.plugins.is_empty());
    }

    #[test]
    fn toggling_a_plugin_dispatches_and_flips_optimistically() {
        let (mut app, mut receiver) = app_with_bridge();
        reduce(
            &mut app,
            &[
                Message::Event(Ev::DeviceFound {
                    id: "a".into(),
                    name: "A".into(),
                }),
                Message::DeviceSelected(0),
            ],
        );
        let _ = receiver.try_recv(); // consume the ListPlugins command

        reduce(
            &mut app,
            &[
                Message::PluginsLoaded(vec![PluginRow {
                    name: "ping".into(),
                    title: "Ping".into(),
                    enabled: false,
                }]),
                Message::TogglePlugin("ping".into(), true),
            ],
        );

        assert!(matches!(
            drained(&mut receiver).first(),
            Some(BridgeIn::SetPlugin { plugin, enabled: true, .. }) if plugin == "ping"
        ));
        assert!(app.plugins[0].enabled);
    }

    #[test]
    fn failed_pair_reverts_state_and_banners() {
        let (mut app, mut receiver) = app_with_bridge();
        reduce(
            &mut app,
            &[
                Message::Event(Ev::DeviceFound {
                    id: "a".into(),
                    name: "A".into(),
                }),
                Message::PairPressed("a".into()),
            ],
        );
        assert!(matches!(drained(&mut receiver).first(),
                Some(BridgeIn::Pair(id)) if id == "a"));
        assert_eq!(app.devices[0].state, "pairing");

        reduce(&mut app, &[Message::Paired(Err("refused".into()))]);
        assert_eq!(app.devices[0].state, "found");
        assert!(!app.devices[0].paired);
        assert!(matches!(&app.banner, Some(text) if text.contains("refused")));
    }

    #[test]
    fn losing_another_device_keeps_selection_stable() {
        let mut app = HandfastApp::new();
        reduce(
            &mut app,
            &[
                Message::Event(Ev::DeviceFound {
                    id: "a".into(),
                    name: "A".into(),
                }),
                Message::Event(Ev::DeviceFound {
                    id: "b".into(),
                    name: "B".into(),
                }),
                Message::DeviceSelected(1),
            ],
        );
        reduce(
            &mut app,
            &[Message::Event(Ev::DeviceLost { id: "a".into() })],
        );
        let selected_name = app
            .selected
            .and_then(|index| app.devices.get(index))
            .map(|card| card.name.clone());
        assert_eq!(selected_name, Some("B".into()));

        // Losing the selected device clears the selection entirely.
        reduce(
            &mut app,
            &[Message::Event(Ev::DeviceLost { id: "b".into() })],
        );
        assert!(app.selected.is_none());
        assert!(app.selected_id.is_none());
    }

    #[test]
    fn successful_pair_marks_card_paired() {
        let (mut app, _receiver) = app_with_bridge();
        reduce(
            &mut app,
            &[
                Message::Event(Ev::DeviceFound {
                    id: "a".into(),
                    name: "A".into(),
                }),
                Message::PairPressed("a".into()),
            ],
        );
        reduce(&mut app, &[Message::Paired(Ok(()))]);
        assert!(app.devices[0].paired);
        assert_eq!(app.devices[0].state, "paired");

        reduce(&mut app, &[Message::UnpairPressed("a".into())]);
        reduce(&mut app, &[Message::Unpaired(Ok(()))]);
        assert!(!app.devices[0].paired);
        assert_eq!(app.devices[0].state, "found");
    }

    #[test]
    fn long_clipboard_lines_are_trimmed_for_the_log() {
        assert_eq!(clamp_line("short", 10), "short");
        let long = clamp_line("abcdefghij", 5);
        assert_eq!(long.chars().count(), 5);
        assert!(long.ends_with("..."));
    }
}
