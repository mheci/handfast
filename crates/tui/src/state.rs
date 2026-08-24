//! Interactive-interface state machine.
//!
//! [`State`] is a plain data structure mutated exclusively through small
//! reducers ([`State::apply_event`], [`State::apply_outcome`],
//! [`State::handle_key`]) so every transition is unit-testable without a
//! terminal. Rendering (see `view.rs`) only ever reads the state.

use std::collections::{HashMap, VecDeque};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use handfast_ipc::ServerEvent;

use crate::model::{DeviceEntry, NotifRow, PluginRow, TransferEntry};

/// Maximum notifications kept by the ring buffer.
pub(crate) const NOTIFICATION_CAP: usize = 200;

/// Maximum log lines kept by the rolling buffer.
pub(crate) const LOG_CAP: usize = 1000;

/// Tabs of the interactive interface, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    /// Known devices plus the selected device's plugin detail panel.
    Devices,
    /// Ongoing and finished file transfers with manual progress bars.
    Transfers,
    /// Notifications mirrored from paired devices (ring buffer).
    Notifications,
    /// Rolling daemon log lines forwarded as events.
    Logs,
    /// Inline keybinding reference.
    Help,
}

impl Tab {
    /// All tabs in display order.
    pub(crate) const ALL: [Self; 5] = [
        Self::Devices,
        Self::Transfers,
        Self::Notifications,
        Self::Logs,
        Self::Help,
    ];

    /// Human-readable tab label.
    #[must_use]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Devices => "Devices",
            Self::Transfers => "Transfers",
            Self::Notifications => "Notifications",
            Self::Logs => "Logs",
            Self::Help => "Help",
        }
    }

    /// Next tab, wrapping around at the end.
    #[must_use]
    pub(crate) fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    /// Previous tab, wrapping around at the start.
    #[must_use]
    pub(crate) fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// Position of `self` within [`Tab::ALL`] (`0` if unlisted).
    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }
}

/// Identity advertised by the daemon in its `Hello` handshake.
#[derive(Debug, Clone)]
pub(crate) struct DaemonIdentity {
    /// Wire protocol version.
    pub(crate) version: u32,
    /// Application name.
    pub(crate) app: String,
    /// Daemon process id.
    pub(crate) pid: u32,
}

/// Side-effectful command requested by a key press, executed by the event
/// loop (`app.rs`) against the IPC client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Leave the interface.
    Quit,
    /// Request pairing with the given device.
    Pair(String),
    /// Revoke pairing with the given device.
    Unpair(String),
    /// Fetch the plugin list for the given device (detail panel opened).
    LoadPlugins(String),
    /// Set one plugin's enabled flag on a device.
    TogglePlugin {
        device_id: String,
        plugin: String,
        enabled: bool,
    },
    /// Send a composed reply to the originator of a mirrored notification
    /// (reply modal confirmed with Enter).
    ReplyToNotification {
        /// Notification being answered.
        notification_id: String,
        /// Reply body as typed in the modal (trimmed, non-empty).
        text: String,
    },
}

/// Result of an asynchronous IPC action spawned by the event loop, funneled
/// back into the reducer through [`State::apply_outcome`].
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Transient status-line message (success or failure text).
    Flash(String),
    /// Fresh authoritative device list.
    Devices(Vec<DeviceEntry>),
    /// Fresh authoritative notification list.
    Notifications(Vec<NotifRow>),
    /// Fresh plugin rows for one device's detail panel.
    Plugins {
        device: String,
        plugins: Vec<PluginRow>,
    },
}

/// Full UI snapshot: lists, cursors, mode flags. Pure data; no handles.
#[derive(Debug)]
pub(crate) struct State {
    /// Active tab.
    pub(crate) tab: Tab,
    /// Help overlay drawn over any tab.
    pub(crate) help_overlay: bool,
    /// Set when the loop should exit after drawing one last frame.
    pub(crate) quit: bool,
    /// Redraw requested since the last frame (event-driven rendering).
    pub(crate) dirty: bool,
    /// Daemon announced shutdown via [`ServerEvent::DaemonShutdown`].
    pub(crate) shutdown: bool,
    /// Identity from the `Hello` handshake, once seen.
    pub(crate) daemon: Option<DaemonIdentity>,
    /// Latest status/feedback line for the footer.
    pub(crate) flash: Option<String>,
    /// Last remote clipboard payload observed.
    pub(crate) clipboard: Option<String>,
    /// Known devices, insertion-ordered by discovery.
    pub(crate) devices: Vec<DeviceEntry>,
    /// Cursor into `devices`.
    pub(crate) device_cursor: usize,
    /// Device detail panel open on the Devices tab.
    pub(crate) detail_open: bool,
    /// Device whose plugins populate the detail panel.
    pub(crate) detail_device: Option<String>,
    /// Plugin rows shown in the detail panel.
    pub(crate) plugins: Vec<PluginRow>,
    /// Cursor into `plugins`.
    pub(crate) plugin_cursor: usize,
    /// Transfer id → tracked entry (name plus byte counters).
    pub(crate) transfers: HashMap<String, TransferEntry>,
    /// Cursor into the sorted transfer ids.
    pub(crate) transfer_cursor: usize,
    /// Notification ring buffer (oldest first), capped at
    /// [`NOTIFICATION_CAP`].
    pub(crate) notifications: VecDeque<NotifRow>,
    /// Cursor into `notifications`.
    pub(crate) notification_cursor: usize,
    /// Reply modal open on the Notifications tab; captures all keys while
    /// set.
    pub(crate) reply_open: bool,
    /// Draft text being composed in the reply modal.
    pub(crate) reply_draft: String,
    /// Id of the notification the draft answers, captured when the modal
    /// opened so ring-buffer churn cannot retarget it.
    pub(crate) reply_target: Option<String>,
    /// Rolling log lines (oldest first), capped at [`LOG_CAP`].
    pub(crate) logs: VecDeque<String>,
}

impl State {
    /// Fresh empty state showing the Devices tab.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            tab: Tab::Devices,
            help_overlay: false,
            quit: false,
            dirty: true,
            shutdown: false,
            daemon: None,
            flash: None,
            clipboard: None,
            devices: Vec::new(),
            device_cursor: 0,
            detail_open: false,
            detail_device: None,
            plugins: Vec::new(),
            plugin_cursor: 0,
            transfers: HashMap::new(),
            transfer_cursor: 0,
            notifications: VecDeque::new(),
            notification_cursor: 0,
            reply_open: false,
            reply_draft: String::new(),
            reply_target: None,
            logs: VecDeque::new(),
        }
    }

    // ---- reducers -------------------------------------------------------

    /// Apply one server-pushed event to the state.
    pub(crate) fn apply_event(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::Hello { version, app, pid } => {
                self.daemon = Some(DaemonIdentity {
                    version: *version,
                    app: app.clone(),
                    pid: *pid,
                });
            }
            ServerEvent::DeviceFound { id, name } => {
                DeviceEntry::upsert(&mut self.devices, DeviceEntry::from_found(id, name));
            }
            ServerEvent::DeviceLost { id } => {
                self.devices.retain(|device| &device.id != id);
                if self.detail_device.as_deref() == Some(id.as_str()) {
                    self.close_detail();
                }
            }
            ServerEvent::DeviceStateChanged { id, state } => {
                if let Some(device) = self.devices.iter_mut().find(|d| &d.id == id) {
                    device.apply_state(state);
                }
            }
            ServerEvent::TransferAdded {
                id,
                file_name,
                total,
                ..
            } => {
                self.transfers
                    .insert(id.clone(), TransferEntry::from_added(id, file_name, *total));
            }
            ServerEvent::TransferProgress {
                id,
                bytes_done,
                bytes_total,
            } => match self.transfers.entry(id.clone()) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    slot.get_mut().apply_progress(*bytes_done, *bytes_total);
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    // Registration predates this session; track by counters.
                    slot.insert(TransferEntry::from_progress(id, *bytes_done, *bytes_total));
                }
            },
            ServerEvent::TransferFinished { id } => {
                if let Some(entry) = self.transfers.get_mut(id) {
                    entry.mark_finished();
                }
            }
            ServerEvent::NotificationReceived {
                id,
                app,
                title,
                body,
            } => {
                self.push_notification(NotifRow {
                    id: id.clone(),
                    app: app.clone(),
                    title: title.clone(),
                    body: body.clone(),
                });
            }
            ServerEvent::ClipboardUpdated { text } => {
                self.clipboard = Some(text.clone());
            }
            ServerEvent::LogRecord { level, msg } => {
                self.push_log(level, msg);
            }
            ServerEvent::DaemonShutdown => {
                self.shutdown = true;
                self.flash = Some("daemon is shutting down".to_owned());
            }
            // Events without UI impact (battery, volume, telephony, …).
            _ => {}
        }
        self.clamp_cursors();
        self.dirty = true;
    }

    /// Apply the result of a backgrounded IPC action.
    pub(crate) fn apply_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Flash(message) => self.flash = Some(message),
            Outcome::Devices(entries) => self.devices = entries,
            Outcome::Notifications(rows) => {
                self.notifications = rows.into_iter().collect();
                while self.notifications.len() > NOTIFICATION_CAP {
                    self.notifications.pop_front();
                }
            }
            Outcome::Plugins { device, plugins } => {
                if self.detail_device.as_deref() == Some(device.as_str()) {
                    self.plugins = plugins;
                }
            }
        }
        self.clamp_cursors();
        self.dirty = true;
    }

    /// Map a terminal key press to navigation/mode changes plus at most one
    /// side-effectful [`Action`] for the event loop to execute.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        // Ctrl+C quits from anywhere, mirroring `q`.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => Some(Action::Quit),
                _ => None,
            };
        }
        // The reply modal captures every key while open so composing text
        // never triggers navigation, tab switches, or a premature quit.
        if self.reply_open {
            return self.handle_reply_key(key);
        }
        match key.code {
            KeyCode::Char('q') => Some(Action::Quit),
            KeyCode::Char('?') => {
                self.help_overlay = !self.help_overlay;
                self.dirty = true;
                None
            }
            KeyCode::Tab => {
                self.tab = self.tab.next();
                self.clamp_cursors();
                self.dirty = true;
                None
            }
            KeyCode::BackTab => {
                self.tab = self.tab.previous();
                self.clamp_cursors();
                self.dirty = true;
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor(1);
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor(-1);
                None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.jump_cursor(false);
                None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.jump_cursor(true);
                None
            }
            KeyCode::Enter => self.toggle_detail(),
            KeyCode::Esc => {
                if self.help_overlay {
                    self.help_overlay = false;
                    self.dirty = true;
                } else if self.detail_open {
                    self.close_detail();
                    self.dirty = true;
                }
                None
            }
            KeyCode::Char('p') => {
                if self.tab == Tab::Devices && !self.devices.is_empty() {
                    self.selected_device_id()
                        .map(str::to_owned)
                        .map(Action::Pair)
                } else {
                    None
                }
            }
            KeyCode::Char('u') => {
                if self.tab == Tab::Devices && !self.devices.is_empty() {
                    self.selected_device_id()
                        .map(str::to_owned)
                        .map(Action::Unpair)
                } else {
                    None
                }
            }
            KeyCode::Char('r') => self.open_reply(),
            KeyCode::Char(' ') => {
                let device = self.detail_device.clone();
                let target = self.selected_plugin_index().and_then(|index| {
                    let row = self.plugins.get(index)?;
                    Some((row.name.clone(), !row.enabled))
                });
                match (device, target) {
                    (Some(device_id), Some((plugin, enabled))) => Some(Action::TogglePlugin {
                        device_id,
                        plugin,
                        enabled,
                    }),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Open/close the Devices-tab detail panel; opening returns a
    /// [`Action::LoadPlugins`] for the selected device.
    fn toggle_detail(&mut self) -> Option<Action> {
        if self.tab != Tab::Devices {
            return None;
        }
        if self.detail_open {
            self.close_detail();
            self.dirty = true;
            return None;
        }
        let selected = self.selected_device_id().map(str::to_owned);
        match selected {
            Some(device_id) => {
                self.detail_open = true;
                self.detail_device = Some(device_id.clone());
                self.plugin_cursor = 0;
                self.plugins.clear();
                self.dirty = true;
                Some(Action::LoadPlugins(device_id))
            }
            None => None,
        }
    }

    /// Close the detail panel and drop its cached rows.
    pub(crate) fn close_detail(&mut self) {
        self.detail_open = false;
        self.detail_device = None;
        self.plugins.clear();
        self.plugin_cursor = 0;
    }

    // ---- reply modal -------------------------------------------------------

    /// Open the reply modal for the notification under the cursor; inert
    /// outside the Notifications tab or without a selection.
    fn open_reply(&mut self) -> Option<Action> {
        let target = if self.tab == Tab::Notifications && !self.notifications.is_empty() {
            self.selected_notification_id().map(str::to_owned)
        } else {
            None
        };
        match target {
            Some(id) => {
                self.reply_open = true;
                self.reply_target = Some(id);
                self.reply_draft.clear();
                self.dirty = true;
                None
            }
            None => None,
        }
    }

    /// Route a key press into the reply-modal text editor.
    ///
    /// Editing is append-only with `Backspace` delete; a trimmed-empty draft
    /// confirmed with Enter cancels instead of sending an empty reply.
    fn handle_reply_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Enter => self.submit_reply(),
            KeyCode::Esc => {
                self.cancel_reply();
                self.dirty = true;
                None
            }
            KeyCode::Backspace => {
                self.reply_draft.pop();
                self.dirty = true;
                None
            }
            KeyCode::Char(ch) => {
                self.reply_draft.push(ch);
                self.dirty = true;
                None
            }
            _ => None,
        }
    }

    /// Confirm the draft: emit [`Action::ReplyToNotification`] for non-empty
    /// text and close the modal either way.
    fn submit_reply(&mut self) -> Option<Action> {
        let target = self.reply_target.clone();
        let text = self.reply_draft.trim().to_owned();
        self.cancel_reply();
        self.dirty = true;
        match (target, text.is_empty()) {
            (Some(notification_id), false) => Some(Action::ReplyToNotification {
                notification_id,
                text,
            }),
            _ => None,
        }
    }

    /// Drop the modal together with its draft and target.
    pub(crate) fn cancel_reply(&mut self) {
        self.reply_open = false;
        self.reply_target = None;
        self.reply_draft.clear();
    }

    /// Notification row the open draft answers, if still buffered.
    #[must_use]
    pub(crate) fn reply_subject(&self) -> Option<&NotifRow> {
        let id = self.reply_target.as_deref()?;
        self.notifications.iter().find(|row| row.id == id)
    }

    /// Record a status/feedback message for the footer.
    pub(crate) fn set_flash(&mut self, message: impl Into<String>) {
        self.flash = Some(message.into());
        self.dirty = true;
    }

    // ---- selection helpers ----------------------------------------------

    /// Id of the device under the cursor.
    #[must_use]
    pub(crate) fn selected_device_id(&self) -> Option<&str> {
        self.devices
            .get(self.device_cursor)
            .map(|device| device.id.as_str())
    }

    /// Id of the notification under the cursor.
    #[must_use]
    pub(crate) fn selected_notification_id(&self) -> Option<&str> {
        self.notifications
            .get(self.notification_cursor)
            .map(|row| row.id.as_str())
    }

    /// Index of the focused plugin row, if the detail panel is active.
    #[must_use]
    pub(crate) fn selected_plugin_index(&self) -> Option<usize> {
        if self.tab == Tab::Devices && self.detail_open {
            self.plugins
                .get(self.plugin_cursor)
                .map(|_| self.plugin_cursor)
        } else {
            None
        }
    }

    /// Transfer rows in stable (id-sorted) order — shared by the renderer and
    /// the cursor logic so highlights always line up.
    #[must_use]
    pub(crate) fn transfer_rows(&self) -> Vec<(&String, &TransferEntry)> {
        let mut rows: Vec<(&String, &TransferEntry)> = self.transfers.iter().collect();
        rows.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        rows
    }

    /// Number of focusable rows for the active view (detail panel steals
    /// focus from the device table while open).
    #[must_use]
    pub(crate) fn focused_len(&self) -> usize {
        match self.tab {
            Tab::Devices => {
                if self.detail_open {
                    self.plugins.len()
                } else {
                    self.devices.len()
                }
            }
            Tab::Transfers => self.transfers.len(),
            Tab::Notifications => self.notifications.len(),
            Tab::Logs | Tab::Help => 0,
        }
    }

    /// Move the active cursor by `delta` rows (-1 or 1), clamped.
    pub(crate) fn move_cursor(&mut self, delta: isize) {
        let len = self.focused_len();
        if len == 0 {
            return;
        }
        let down = delta > 0;
        let tab = self.tab;
        let detail_open = self.detail_open;
        let cursor = match (tab, detail_open) {
            (Tab::Devices, true) => &mut self.plugin_cursor,
            (Tab::Devices, false) => &mut self.device_cursor,
            (Tab::Transfers, _) => &mut self.transfer_cursor,
            (Tab::Notifications, _) => &mut self.notification_cursor,
            (Tab::Logs | Tab::Help, _) => return,
        };
        *cursor = step(*cursor, down, len);
        self.dirty = true;
    }

    /// Jump the active cursor to the first (`last == false`) or last row.
    pub(crate) fn jump_cursor(&mut self, last: bool) {
        let len = self.focused_len();
        if len == 0 {
            return;
        }
        let tab = self.tab;
        let detail_open = self.detail_open;
        let cursor = match (tab, detail_open) {
            (Tab::Devices, true) => &mut self.plugin_cursor,
            (Tab::Devices, false) => &mut self.device_cursor,
            (Tab::Transfers, _) => &mut self.transfer_cursor,
            (Tab::Notifications, _) => &mut self.notification_cursor,
            (Tab::Logs | Tab::Help, _) => return,
        };
        *cursor = if last { len - 1 } else { 0 };
        self.dirty = true;
    }

    /// Keep every cursor inside its list after removals/replacements.
    pub(crate) fn clamp_cursors(&mut self) {
        self.device_cursor = clamp_at(self.device_cursor, self.devices.len());
        self.plugin_cursor = clamp_at(self.plugin_cursor, self.plugins.len());
        self.transfer_cursor = clamp_at(self.transfer_cursor, self.transfers.len());
        self.notification_cursor = clamp_at(self.notification_cursor, self.notifications.len());
    }

    // ---- capped buffers ---------------------------------------------------

    /// Append one formatted log line, dropping the oldest beyond [`LOG_CAP`].
    pub(crate) fn push_log(&mut self, level: &str, msg: &str) {
        self.logs.push_back(format!("[{level:<5}] {msg}"));
        while self.logs.len() > LOG_CAP {
            self.logs.pop_front();
        }
    }

    /// Append one notification, dropping the oldest beyond
    /// [`NOTIFICATION_CAP`].
    pub(crate) fn push_notification(&mut self, row: NotifRow) {
        self.notifications.push_back(row);
        while self.notifications.len() > NOTIFICATION_CAP {
            self.notifications.pop_front();
        }
    }
}

/// Clamp `cursor` into `0..len` (collapses to `0` for empty lists).
fn clamp_at(cursor: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        cursor.min(len - 1)
    }
}

/// One-step cursor movement without underflow/overflow.
fn step(cursor: usize, down: bool, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let max = len - 1;
    match (down, cursor) {
        (true, current) if current < max => current + 1,
        (true, _) => max,
        (false, 0) => 0,
        (false, current) => current - 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    /// Press-style key event with no modifiers.
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Ctrl-modified press (e.g. Ctrl+C).
    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Release-kind variant used to assert Windows release events are ignored.
    fn released(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        }
    }

    fn found(id: &str, name: &str) -> ServerEvent {
        ServerEvent::DeviceFound {
            id: id.to_owned(),
            name: name.to_owned(),
        }
    }

    fn notif(id: &str) -> ServerEvent {
        ServerEvent::NotificationReceived {
            id: id.to_owned(),
            app: "app".to_owned(),
            title: "t".to_owned(),
            body: "b".to_owned(),
        }
    }

    fn log(level: &str, msg: &str) -> ServerEvent {
        ServerEvent::LogRecord {
            level: level.to_owned(),
            msg: msg.to_owned(),
        }
    }

    #[test]
    fn device_found_upserts_without_duplicates() {
        let mut state = State::new();
        state.apply_event(&found("a", "Alpha"));
        state.apply_event(&found("b", "Beta"));
        state.apply_event(&found("a", "Alpha2"));
        assert_eq!(state.devices.len(), 2);
        assert_eq!(state.devices[0].name, "Alpha2");
        assert_eq!(state.devices[1].id, "b");
    }

    #[test]
    fn device_lost_removes_and_clamps_cursor() {
        let mut state = State::new();
        for id in ["a", "b", "c"] {
            state.apply_event(&found(id, id));
        }
        state.apply_event(&ServerEvent::DeviceStateChanged {
            id: "c".to_owned(),
            state: "paired".to_owned(),
        });
        state.handle_key(key(KeyCode::End));
        assert_eq!(state.selected_device_id(), Some("c"));
        state.apply_event(&ServerEvent::DeviceLost { id: "c".to_owned() });
        assert_eq!(state.devices.len(), 2);
        assert_eq!(state.selected_device_id(), Some("b"));
        state.apply_event(&ServerEvent::DeviceLost { id: "a".to_owned() });
        state.apply_event(&ServerEvent::DeviceLost { id: "b".to_owned() });
        assert_eq!(state.selected_device_id(), None);
    }

    #[test]
    fn state_changed_drives_pairing_badge() {
        let mut state = State::new();
        state.apply_event(&found("a", "Alpha"));
        state.apply_event(&ServerEvent::DeviceStateChanged {
            id: "a".to_owned(),
            state: "paired".to_owned(),
        });
        assert!(state.devices[0].paired);
        assert_eq!(state.devices[0].state, "paired");
        state.apply_event(&ServerEvent::DeviceStateChanged {
            id: "a".to_owned(),
            state: "reachable".to_owned(),
        });
        assert!(!state.devices[0].paired);
    }

    #[test]
    fn transfer_added_registers_named_entry() {
        let mut state = State::new();
        state.apply_event(&ServerEvent::TransferAdded {
            id: "t1".to_owned(),
            device_id: "d1".to_owned(),
            direction: "outgoing".to_owned(),
            file_name: "photo.jpg".to_owned(),
            total: 100,
        });
        assert_eq!(state.transfers.len(), 1);
        let entry = &state.transfers["t1"];
        assert_eq!(entry.name, "photo.jpg");
        assert_eq!(entry.done, 0);
        assert_eq!(entry.total, 100);
        assert!(!entry.is_finished());
    }

    #[test]
    fn transfer_progress_upserts_by_id() {
        let mut state = State::new();
        state.apply_event(&ServerEvent::TransferProgress {
            id: "t1".to_owned(),
            bytes_done: 10,
            bytes_total: 100,
        });
        state.apply_event(&ServerEvent::TransferProgress {
            id: "t1".to_owned(),
            bytes_done: 50,
            bytes_total: 100,
        });
        state.apply_event(&ServerEvent::TransferProgress {
            id: "t2".to_owned(),
            bytes_done: 1,
            bytes_total: 4,
        });
        assert_eq!(state.transfers.len(), 2);
        assert_eq!(state.transfers["t1"].done, 50);
        assert_eq!(state.transfers["t1"].total, 100);
        // Rows render in id order for cursor alignment.
        assert_eq!(state.transfer_rows()[0].0, "t1");
    }

    #[test]
    fn added_then_progress_then_finished_is_the_happy_path() {
        let mut state = State::new();
        state.apply_event(&ServerEvent::TransferAdded {
            id: "t1".to_owned(),
            device_id: "d1".to_owned(),
            direction: "incoming".to_owned(),
            file_name: "movie.mkv".to_owned(),
            total: 1000,
        });
        state.apply_event(&ServerEvent::TransferProgress {
            id: "t1".to_owned(),
            bytes_done: 250,
            bytes_total: 1000,
        });
        let entry = &state.transfers["t1"];
        assert_eq!(entry.name, "movie.mkv");
        assert_eq!(entry.done, 250);
        state.apply_event(&ServerEvent::TransferFinished {
            id: "t1".to_owned(),
        });
        let entry = &state.transfers["t1"];
        // Finished transfers stay listed with a full bar.
        assert_eq!(entry.done, 1000);
        assert!(entry.is_finished());
        assert_eq!(state.transfers.len(), 1);
    }

    #[test]
    fn finished_without_known_size_keeps_counters() {
        let mut state = State::new();
        state.apply_event(&ServerEvent::TransferAdded {
            id: "t9".to_owned(),
            device_id: "d".to_owned(),
            direction: "incoming".to_owned(),
            file_name: "blob.bin".to_owned(),
            total: 0,
        });
        state.apply_event(&ServerEvent::TransferProgress {
            id: "t9".to_owned(),
            bytes_done: 4096,
            bytes_total: 0,
        });
        state.apply_event(&ServerEvent::TransferFinished {
            id: "t9".to_owned(),
        });
        let entry = &state.transfers["t9"];
        assert_eq!(entry.done, 4096);
        assert!(!entry.is_finished());
    }

    #[test]
    fn notification_ring_buffer_caps_at_limit() {
        let mut state = State::new();
        for i in 0..(NOTIFICATION_CAP + 5) {
            state.apply_event(&notif(&format!("n{i}")));
        }
        assert_eq!(state.notifications.len(), NOTIFICATION_CAP);
        // Oldest five were dropped: front is now n5, back is n204.
        assert_eq!(
            state.notifications.front().map(|r| r.id.as_str()),
            Some("n5")
        );
        assert_eq!(
            state.notifications.back().map(|r| r.id.as_str()),
            Some(format!("n{}", NOTIFICATION_CAP + 4).as_str())
        );
    }

    #[test]
    fn log_ring_buffer_caps_at_limit_and_formats_lines() {
        let mut state = State::new();
        for i in 0..(LOG_CAP + 10) {
            state.apply_event(&log("INFO", &format!("m{i}")));
        }
        assert_eq!(state.logs.len(), LOG_CAP);
        assert_eq!(state.logs.front().map(String::as_str), Some("[INFO ] m10"));
        assert_eq!(state.logs.back().map(String::as_str), Some("[INFO ] m1009"));
    }

    #[test]
    fn hello_and_clipboard_are_recorded() {
        let mut state = State::new();
        state.apply_event(&ServerEvent::Hello {
            version: 7,
            app: "handfastd".to_owned(),
            pid: 4242,
        });
        let daemon = state.daemon.as_ref();
        assert_eq!(daemon.map(|d| d.version), Some(7));
        assert_eq!(daemon.map(|d| d.pid), Some(4242));
        state.apply_event(&ServerEvent::ClipboardUpdated {
            text: "hi".to_owned(),
        });
        assert_eq!(state.clipboard.as_deref(), Some("hi"));
    }

    #[test]
    fn shutdown_sets_flag_once() {
        let mut state = State::new();
        state.apply_event(&ServerEvent::DaemonShutdown);
        assert!(state.shutdown);
    }

    #[test]
    fn tabs_cycle_in_both_directions() {
        let mut state = State::new();
        assert_eq!(state.tab, Tab::Devices);
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.tab, Tab::Transfers);
        state.handle_key(key(KeyCode::BackTab));
        assert_eq!(state.tab, Tab::Devices);
        state.handle_key(key(KeyCode::BackTab));
        assert_eq!(state.tab, Tab::Help);
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.tab, Tab::Devices);
    }

    #[test]
    fn navigation_moves_and_clamps() {
        let mut state = State::new();
        for id in ["a", "b", "c"] {
            state.apply_event(&found(id, id));
        }
        assert_eq!(state.selected_device_id(), Some("a"));
        state.handle_key(key(KeyCode::Char('j')));
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.selected_device_id(), Some("c"));
        // Further moves clamp at the last row.
        state.handle_key(key(KeyCode::Char('j')));
        state.handle_key(key(KeyCode::Char('k')));
        assert_eq!(state.selected_device_id(), Some("b"));
        state.handle_key(key(KeyCode::Char('g')));
        assert_eq!(state.selected_device_id(), Some("a"));
        state.handle_key(key(KeyCode::Char('G')));
        assert_eq!(state.selected_device_id(), Some("c"));
    }

    #[test]
    fn pair_and_unpair_require_a_selected_device_on_devices_tab() {
        let mut state = State::new();
        assert_eq!(state.handle_key(key(KeyCode::Char('p'))), None);
        state.apply_event(&found("a", "Alpha"));
        assert_eq!(
            state.handle_key(key(KeyCode::Char('p'))),
            Some(Action::Pair("a".to_owned()))
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('u'))),
            Some(Action::Unpair("a".to_owned()))
        );
        // Same keys are inert on other tabs.
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.handle_key(key(KeyCode::Char('p'))), None);
    }

    #[test]
    fn enter_opens_detail_and_requests_plugins() {
        let mut state = State::new();
        state.apply_event(&found("dev", "Device"));
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Some(Action::LoadPlugins("dev".to_owned()))
        );
        assert!(state.detail_open);
        state.apply_outcome(Outcome::Plugins {
            device: "dev".to_owned(),
            plugins: vec![
                PluginRow {
                    name: "ping".into(),
                    title: "Ping".into(),
                    enabled: false,
                },
                PluginRow {
                    name: "sms".into(),
                    title: "SMS".into(),
                    enabled: true,
                },
            ],
        });
        assert_eq!(state.plugins.len(), 2);
        // Enter again closes the panel.
        assert_eq!(state.handle_key(key(KeyCode::Enter)), None);
        assert!(!state.detail_open);
        assert!(state.plugins.is_empty());
    }

    #[test]
    fn space_toggles_only_inside_an_open_detail_panel() {
        let mut state = State::new();
        state.apply_event(&found("dev", "Device"));
        let _ = state.handle_key(key(KeyCode::Enter));
        state.apply_outcome(Outcome::Plugins {
            device: "dev".to_owned(),
            plugins: vec![PluginRow {
                name: "ping".into(),
                title: "Ping".into(),
                enabled: false,
            }],
        });
        let expected = Action::TogglePlugin {
            device_id: "dev".to_owned(),
            plugin: "ping".to_owned(),
            enabled: true,
        };
        assert_eq!(state.handle_key(key(KeyCode::Char(' '))), Some(expected));
        // Detail closed: space does nothing.
        state.close_detail();
        assert_eq!(state.handle_key(key(KeyCode::Char(' '))), None);
    }

    #[test]
    fn r_opens_reply_modal_only_on_notifications_tab() {
        let mut state = State::new();
        state.apply_event(&found("dev", "Device"));
        assert_eq!(state.handle_key(key(KeyCode::Char('r'))), None);
        assert!(!state.reply_open);

        state.apply_event(&notif("n1"));
        state.handle_key(key(KeyCode::Tab));
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.tab, Tab::Notifications);
        assert_eq!(state.handle_key(key(KeyCode::Char('r'))), None);
        assert!(state.reply_open);
        assert_eq!(state.reply_target.as_deref(), Some("n1"));
        assert!(state.reply_draft.is_empty());
    }

    #[test]
    fn reply_modal_types_edits_and_sends_on_enter() {
        let mut state = State::new();
        state.apply_event(&notif("n1"));
        state.handle_key(key(KeyCode::Tab));
        state.handle_key(key(KeyCode::Tab));
        let _ = state.handle_key(key(KeyCode::Char('r')));
        for ch in "hey!".chars() {
            let _ = state.handle_key(key(KeyCode::Char(ch)));
        }
        let _ = state.handle_key(key(KeyCode::Backspace));
        assert_eq!(state.reply_draft, "hey");
        let action = state.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            Some(Action::ReplyToNotification {
                notification_id: "n1".to_owned(),
                text: "hey".to_owned(),
            })
        );
        assert!(!state.reply_open);
        assert_eq!(state.reply_target, None);
        assert!(state.reply_draft.is_empty());
    }

    #[test]
    fn blank_reply_confirms_as_cancel_without_action() {
        let mut state = State::new();
        state.apply_event(&notif("n1"));
        state.handle_key(key(KeyCode::Tab));
        state.handle_key(key(KeyCode::Tab));
        let _ = state.handle_key(key(KeyCode::Char('r')));
        for ch in "   ".chars() {
            let _ = state.handle_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(state.handle_key(key(KeyCode::Enter)), None);
        assert!(!state.reply_open);
        // Esc also cancels and drops the draft.
        let _ = state.handle_key(key(KeyCode::Char('r')));
        let _ = state.handle_key(key(KeyCode::Char('x')));
        let _ = state.handle_key(key(KeyCode::Esc));
        assert!(!state.reply_open);
        assert!(state.reply_draft.is_empty());
        assert_eq!(state.reply_target, None);
    }

    #[test]
    fn reply_modal_captures_all_other_keys() {
        let mut state = State::new();
        state.apply_event(&notif("n1"));
        // Devices → Transfers → Notifications.
        state.handle_key(key(KeyCode::Tab));
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.tab, Tab::Notifications);
        let _ = state.handle_key(key(KeyCode::Char('r')));

        // While composing: quit, help, tab switch, navigation are swallowed…
        for code in [
            KeyCode::Char('q'),
            KeyCode::Char('?'),
            KeyCode::Tab,
            KeyCode::Char('j'),
            KeyCode::Char('p'),
        ] {
            assert_eq!(state.handle_key(key(code)), None, "swallowed {code:?}");
        }
        // …and none of them mutated the surrounding UI.
        assert!(!state.help_overlay);
        assert_eq!(state.tab, Tab::Notifications);
        assert_eq!(state.notification_cursor, 0);
        assert_eq!(state.reply_draft, "q?jp"); // only printable chars landed

        // Ctrl+C still quits from inside the modal.
        assert_eq!(
            state.handle_key(ctrl(KeyCode::Char('c'))),
            Some(Action::Quit)
        );
    }

    #[test]
    fn reply_target_is_pinned_against_ring_buffer_churn() {
        let mut state = State::new();
        for i in 0..(NOTIFICATION_CAP + 1) {
            state.apply_event(&notif(&format!("n{i}")));
        }
        state.handle_key(key(KeyCode::Tab));
        state.handle_key(key(KeyCode::Tab));
        state.handle_key(key(KeyCode::End));
        let first_visible = state.selected_notification_id().map(str::to_owned);
        let _ = state.handle_key(key(KeyCode::Char('r')));
        // Overflow the ring buffer while the modal is open.
        state.apply_event(&notif("late"));
        for ch in "on my way".chars() {
            let _ = state.handle_key(key(KeyCode::Char(ch)));
        }
        let action = state.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            Some(Action::ReplyToNotification {
                notification_id: first_visible.expect("selection existed"),
                text: "on my way".to_owned(),
            }),
            "the target is pinned at modal-open time"
        );
    }

    #[test]
    fn esc_closes_overlay_then_detail() {
        let mut state = State::new();
        state.apply_event(&found("dev", "Device"));
        let _ = state.handle_key(key(KeyCode::Enter));
        state.handle_key(key(KeyCode::Char('?')));
        assert!(state.help_overlay);
        state.handle_key(key(KeyCode::Esc));
        assert!(!state.help_overlay);
        assert!(state.detail_open);
        state.handle_key(key(KeyCode::Esc));
        assert!(!state.detail_open);
    }

    #[test]
    fn quit_via_q_and_ctrl_c_only_on_press_kind() {
        let mut state = State::new();
        assert_eq!(
            state.handle_key(key(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
        assert_eq!(
            state.handle_key(ctrl(KeyCode::Char('c'))),
            Some(Action::Quit)
        );
        // Windows sends release events; those must be ignored everywhere.
        assert_eq!(state.handle_key(released(KeyCode::Char('q'))), None);
    }

    #[test]
    fn outcomes_refresh_lists() {
        let mut state = State::new();
        state.apply_outcome(Outcome::Flash("hello".to_owned()));
        assert_eq!(state.flash.as_deref(), Some("hello"));
        state.apply_outcome(Outcome::Devices(vec![DeviceEntry::from_found("x", "X")]));
        assert_eq!(state.devices.len(), 1);
        state.apply_outcome(Outcome::Notifications(vec![NotifRow {
            id: "n".into(),
            app: String::new(),
            title: String::new(),
            body: String::new(),
        }]));
        assert_eq!(state.notifications.len(), 1);
        // Plugin payloads for other devices are ignored.
        state.apply_outcome(Outcome::Plugins {
            device: "other".to_owned(),
            plugins: vec![PluginRow {
                name: "nope".into(),
                title: String::new(),
                enabled: false,
            }],
        });
        assert!(state.plugins.is_empty());
    }
}
