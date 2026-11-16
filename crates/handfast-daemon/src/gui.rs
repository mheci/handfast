//! The Handfast window.
//!
//! The window never opens the store. A worker thread owns the control client
//! and the child the window started: it polls `Status` once a second, applies
//! the Start and Stop commands, and sends each snapshot to the UI. The UI
//! thread only draws the latest snapshot, so a stopped daemon or a slow control
//! reply cannot freeze the window.
//!
//! Start spawns `handfast daemon` as a child. Stop is always the `Shutdown`
//! control request, because a daemon started by the logon task or the systemd
//! unit is not this process's child and still has to be stoppable.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use eframe::egui;
use handfast_proto::DeviceId;
use handfast_runtime::RemoteMediaAction;

use crate::control::{ControlError, DaemonStatus, RemoteMediaCommand};
use crate::service::{DaemonControl, LocalControl};

const POLL_INTERVAL: Duration = Duration::from_secs(1);

const LOG_FILE: &str = "handfast.log";

const ICON_RGBA: &[u8] = include_bytes!("../../../assets/branding/handfast-128.rgba");
const ICON_SIZE: u32 = 128;

fn icon() -> egui::IconData {
    egui::IconData {
        rgba: ICON_RGBA.to_vec(),
        width: ICON_SIZE,
        height: ICON_SIZE,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DaemonState {
    Unknown,
    Stopped,
    Running(Box<DaemonStatus>),
    Unreachable(String),
}

fn reduce(result: Result<DaemonStatus, ControlError>) -> DaemonState {
    match result {
        Ok(status) => DaemonState::Running(Box::new(status)),
        Err(ControlError::NotRunning) => DaemonState::Stopped,
        Err(error) => DaemonState::Unreachable(error.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerCommand {
    Start,
    Stop,
    Refresh,
    Ring {
        device_id: DeviceId,
    },
    RemoteVolume {
        device_id: DeviceId,
        sink: String,
        percent: Option<u8>,
        muted: Option<bool>,
    },
    Media {
        device_id: DeviceId,
        command: RemoteMediaCommand,
    },
    PairingDecision {
        device_id: DeviceId,
        accept: bool,
        fingerprint: String,
    },
}

fn daemon_command(exe: &Path, data_dir: &Path) -> Command {
    let mut command = Command::new(exe);
    command
        .arg("daemon")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--log-file")
        .arg(data_dir.join(LOG_FILE));
    #[cfg(windows)]
    {
        // A console child of a GUI process would open its own console window.
        // The log file carries the daemon's diagnostics instead.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn spawn_daemon(exe: &Path, data_dir: &Path) -> io::Result<Child> {
    daemon_command(exe, data_dir).spawn()
}

fn media_action(device_id: DeviceId, player: String, action: RemoteMediaAction) -> WorkerCommand {
    WorkerCommand::Media {
        device_id,
        command: RemoteMediaCommand::Action { player, action },
    }
}

fn spawn_worker(exe: PathBuf, data_dir: PathBuf) -> (Sender<WorkerCommand>, Receiver<DaemonState>) {
    let (commands, command_rx) = mpsc::channel();
    let (states, state_rx) = mpsc::channel();
    std::thread::spawn(move || worker(&exe, &data_dir, &command_rx, &states));
    (commands, state_rx)
}

fn worker(
    exe: &Path,
    data_dir: &Path,
    commands: &Receiver<WorkerCommand>,
    states: &Sender<DaemonState>,
) {
    let control = match LocalControl::new(data_dir.to_path_buf()) {
        Ok(control) => control,
        Err(error) => {
            let _ = states.send(DaemonState::Unreachable(format!(
                "cannot build the control client: {error}"
            )));
            return;
        }
    };
    let mut child: Option<Child> = None;
    loop {
        if let Some(spawned) = child.as_mut() {
            if !matches!(spawned.try_wait(), Ok(None)) {
                child = None;
            }
        }
        if states.send(reduce(control.status())).is_err() {
            break;
        }
        match commands.recv_timeout(POLL_INTERVAL) {
            Ok(WorkerCommand::Start) => {
                if child.is_none() && control.status().is_err() {
                    match spawn_daemon(exe, data_dir) {
                        Ok(spawned) => child = Some(spawned),
                        Err(error) => eprintln!("handfast: cannot start the daemon: {error}"),
                    }
                }
            }
            Ok(WorkerCommand::Stop) => {
                if let Err(error) = control.shutdown() {
                    eprintln!("handfast: cannot stop the daemon: {error}");
                }
            }
            Ok(WorkerCommand::Refresh) | Err(RecvTimeoutError::Timeout) => {}
            Ok(WorkerCommand::Ring { device_id }) => {
                if let Err(error) = control.ring(&device_id) {
                    eprintln!("handfast: cannot ring the device: {error}");
                }
            }
            Ok(WorkerCommand::RemoteVolume {
                device_id,
                sink,
                percent,
                muted,
            }) => {
                if let Err(error) = control.set_remote_volume(&device_id, &sink, percent, muted) {
                    eprintln!("handfast: cannot change the remote volume: {error}");
                }
            }
            Ok(WorkerCommand::Media { device_id, command }) => {
                if let Err(error) = control.media(&device_id, command) {
                    eprintln!("handfast: cannot send the media command: {error}");
                }
            }
            Ok(WorkerCommand::PairingDecision {
                device_id,
                accept,
                fingerprint,
            }) => {
                if let Err(error) = control.pairing_decision(&device_id, accept, &fingerprint) {
                    eprintln!("handfast: cannot answer the pairing request: {error}");
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Open the window and drive the daemon until the user closes it.
///
/// `hide_console` hides the Windows console window when this process is the
/// only one attached, so a double-clicked exe shows a window and nothing else.
pub fn run(data_dir: PathBuf, hide_console: bool) -> ExitCode {
    #[cfg(windows)]
    if hide_console {
        hide_console_when_alone();
    }
    #[cfg(not(windows))]
    let _ = hide_console;

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("handfast: cannot resolve the executable path: {error}");
            return ExitCode::FAILURE;
        }
    };
    let (commands, states) = spawn_worker(exe, data_dir);
    let app = HandfastApp {
        commands,
        states,
        state: DaemonState::Unknown,
        pending_volumes: std::collections::HashMap::new(),
        selected_players: std::collections::HashMap::new(),
        pending_media_volumes: std::collections::HashMap::new(),
        pending_positions: std::collections::HashMap::new(),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 620.0])
            .with_icon(icon()),
        ..Default::default()
    };
    match eframe::run_native("Handfast", options, Box::new(move |_cc| Ok(Box::new(app)))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("handfast: the window failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// A double-clicked `handfast.exe` gets a fresh console with one attached
/// process. A launch from a terminal attaches the shell too, so the console
/// stays and `handfast daemon` keeps logging to it.
#[cfg(windows)]
#[allow(unsafe_code)]
fn hide_console_when_alone() {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

    let mut processes = [0u32; 2];
    // SAFETY: `processes` is a live buffer of the length passed with it, and
    // the call only writes process ids into it.
    let attached = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) };
    if attached != 1 {
        return;
    }
    // SAFETY: the handle is the console window of this process, and
    // `ShowWindow` returns zero rather than faulting on a null handle.
    unsafe { ShowWindow(GetConsoleWindow(), SW_HIDE) };
}

fn open_command(path: &Path) -> Command {
    #[cfg(windows)]
    let program = "explorer.exe";
    #[cfg(not(windows))]
    let program = "xdg-open";
    let mut command = Command::new(program);
    command.arg(path);
    command
}

fn open_folder(path: &Path) {
    let mut command = open_command(path);
    let program = command.get_program().to_owned();
    match command.spawn() {
        Ok(mut child) => {
            // The opener hands off and exits, and a Linux click would otherwise
            // leave a zombie. The wait runs off the UI thread.
            std::thread::spawn(move || match child.wait() {
                Ok(status) if !status.success() => {
                    eprintln!(
                        "handfast: {} exited with {status}",
                        program.to_string_lossy()
                    );
                }
                Ok(_) => {}
                Err(error) => eprintln!("handfast: cannot reap the opener: {error}"),
            });
        }
        Err(error) => eprintln!("handfast: cannot open {}: {error}", path.display()),
    }
}

struct HandfastApp {
    commands: Sender<WorkerCommand>,
    states: Receiver<DaemonState>,
    state: DaemonState,
    pending_volumes: std::collections::HashMap<(String, String), u8>,
    selected_players: std::collections::HashMap<String, String>,
    pending_media_volumes: std::collections::HashMap<(String, String), u8>,
    pending_positions: std::collections::HashMap<(String, String), i64>,
}

impl HandfastApp {
    fn send(&self, command: WorkerCommand) {
        let _ = self.commands.send(command);
    }

    fn draw(&mut self, ctx: &egui::Context) {
        while let Ok(state) = self.states.try_recv() {
            self.state = state;
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.header(ui);
                ui.separator();
                self.identity(ui);
                ui.separator();
                self.pairings(ui);
                ui.separator();
                self.devices(ui);
                ui.separator();
                self.transfers(ui);
            });
        });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let running = matches!(&self.state, DaemonState::Running(_));
        let incoming = match &self.state {
            DaemonState::Running(status) => Some(status.incoming_dir.clone()),
            _ => None,
        };

        ui.horizontal(|ui| {
            ui.heading(status_label(&self.state));
            if let DaemonState::Running(status) = &self.state {
                ui.label(format!("handfast {}", status.version));
                ui.label(format!("pid {}", status.pid));
                ui.label(format!("uptime {}s", status.uptime_secs));
            }
            if let DaemonState::Unreachable(message) = &self.state {
                ui.label(message);
            }
        });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!running, egui::Button::new("Start"))
                .clicked()
            {
                self.send(WorkerCommand::Start);
            }
            if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                self.send(WorkerCommand::Stop);
            }
            if ui
                .add_enabled(
                    incoming.is_some(),
                    egui::Button::new("Open incoming folder"),
                )
                .clicked()
            {
                if let Some(path) = &incoming {
                    open_folder(path);
                }
            }
            if ui.button("Refresh").clicked() {
                self.send(WorkerCommand::Refresh);
            }
        });
        if let Some(path) = &incoming {
            ui.label(format!("incoming: {}", path.display()));
        }
    }

    fn identity(&mut self, ui: &mut egui::Ui) {
        ui.heading("This device");
        let Some(status) = running_status(&self.state) else {
            ui.label("Start the daemon to read the identity.");
            return;
        };
        egui::Grid::new("identity").num_columns(2).show(ui, |ui| {
            ui.label("Name");
            ui.label(&status.identity.device_name);
            ui.end_row();

            ui.label("Device ID");
            ui.monospace(status.identity.device_id.to_string());
            ui.end_row();

            ui.label("Fingerprint");
            ui.horizontal(|ui| {
                ui.monospace(&status.identity.fingerprint);
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(status.identity.fingerprint.clone());
                }
            });
            ui.end_row();

            ui.label("TCP port");
            ui.label(status.identity.tcp_port.to_string());
            ui.end_row();
        });
    }

    fn pairings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Pairing requests");
        let Some(status) = running_status(&self.state) else {
            ui.label("Start the daemon to answer pairing requests.");
            return;
        };
        if status.pending_pairings.is_empty() {
            ui.label("No pairing requests.");
            return;
        }
        egui::Grid::new("pairings")
            .num_columns(4)
            .striped(true)
            .show(ui, |ui| {
                ui.label("Name");
                ui.label("Device ID");
                ui.label("Fingerprint");
                ui.label("Answer");
                ui.end_row();
                for pairing in &status.pending_pairings {
                    ui.label(&pairing.device_name);
                    ui.monospace(pairing.device_id.to_string());
                    ui.monospace(&pairing.fingerprint);
                    ui.horizontal(|ui| {
                        if ui.button("Accept").clicked() {
                            self.send(WorkerCommand::PairingDecision {
                                device_id: pairing.device_id.clone(),
                                accept: true,
                                fingerprint: pairing.fingerprint.clone(),
                            });
                        }
                        if ui.button("Reject").clicked() {
                            self.send(WorkerCommand::PairingDecision {
                                device_id: pairing.device_id.clone(),
                                accept: false,
                                fingerprint: pairing.fingerprint.clone(),
                            });
                        }
                    });
                    ui.end_row();
                }
            });
    }

    fn devices(&mut self, ui: &mut egui::Ui) {
        ui.heading("Paired devices");
        let Some(status) = running_status(&self.state) else {
            ui.label("Start the daemon to list paired devices.");
            return;
        };
        if status.paired.is_empty() {
            ui.label("No paired devices.");
            return;
        }
        egui::Grid::new("paired")
            .num_columns(5)
            .striped(true)
            .show(ui, |ui| {
                ui.label("Name");
                ui.label("Device ID");
                ui.label("Type");
                ui.label("Link");
                ui.label("Actions");
                ui.end_row();
                for device in &status.paired {
                    ui.label(&device.device_name);
                    ui.monospace(device.device_id.to_string());
                    ui.label(&device.device_type);
                    ui.label(if device.connected {
                        "connected"
                    } else {
                        "offline"
                    });
                    if ui
                        .add_enabled(device.connected, egui::Button::new("Ring"))
                        .clicked()
                    {
                        self.send(WorkerCommand::Ring {
                            device_id: device.device_id.clone(),
                        });
                    }
                    ui.end_row();
                }
            });
        if status.truncated {
            ui.label("The list is truncated.");
        }
        let remote_sinks = status.remote_sinks.clone();
        let remote_players = status.remote_players.clone();
        ui.label("Send a file from a terminal with handfast share --device <id> <path>.");
        self.remote_volume(ui, &remote_sinks);
        self.remote_media(ui, &remote_players);
    }

    fn remote_volume(&mut self, ui: &mut egui::Ui, devices: &[crate::control::RemoteDeviceSinks]) {
        if devices.is_empty() {
            return;
        }
        ui.separator();
        ui.heading("Remote audio");
        self.pending_volumes.retain(|(device, sink), percent| {
            devices.iter().any(|reported| {
                reported.device_id.as_str() == device
                    && reported
                        .sinks
                        .iter()
                        .any(|reported| &reported.name == sink && reported.percent() == *percent)
            })
        });
        let mut commands = Vec::new();
        for device in devices {
            ui.label(format!("Device {}", device.device_id));
            for sink in &device.sinks {
                let key = (device.device_id.to_string(), sink.name.clone());
                let mut percent = *self.pending_volumes.get(&key).unwrap_or(&sink.percent());
                ui.horizontal(|ui| {
                    let label = if sink.description.is_empty() {
                        sink.name.clone()
                    } else {
                        format!("{} ({})", sink.description, sink.name)
                    };
                    let slider = ui.add(egui::Slider::new(&mut percent, 0..=100).text(label));
                    if slider.changed() {
                        self.pending_volumes.insert(key.clone(), percent);
                    }
                    if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                        commands.push(WorkerCommand::RemoteVolume {
                            device_id: device.device_id.clone(),
                            sink: sink.name.clone(),
                            percent: Some(percent),
                            muted: None,
                        });
                    }
                    let mut muted = sink.muted;
                    if ui.checkbox(&mut muted, "Mute").changed() {
                        commands.push(WorkerCommand::RemoteVolume {
                            device_id: device.device_id.clone(),
                            sink: sink.name.clone(),
                            percent: None,
                            muted: Some(muted),
                        });
                    }
                    if sink.enabled {
                        ui.label("default");
                    }
                });
            }
        }
        for command in commands {
            self.send(command);
        }
    }

    fn remote_media(&mut self, ui: &mut egui::Ui, devices: &[crate::control::RemoteDevicePlayers]) {
        if devices.is_empty() {
            return;
        }
        ui.separator();
        ui.heading("Remote media");
        self.pending_media_volumes
            .retain(|(device, player), volume| {
                devices.iter().any(|reported| {
                    reported.device_id.as_str() == device
                        && reported
                            .players
                            .iter()
                            .any(|model| &model.player == player && model.volume == *volume)
                })
            });
        self.pending_positions.retain(|(device, player), position| {
            devices.iter().any(|reported| {
                reported.device_id.as_str() == device
                    && reported.players.iter().any(|model| {
                        &model.player == player && (model.position_ms - *position).abs() < 1000
                    })
            })
        });
        let mut commands = Vec::new();
        for device in devices {
            if device.players.is_empty() {
                continue;
            }
            let key = device.device_id.to_string();
            let mut selected = self
                .selected_players
                .get(&key)
                .cloned()
                .unwrap_or_else(|| device.players[0].player.clone());
            if !device.players.iter().any(|model| model.player == selected) {
                selected = device.players[0].player.clone();
            }
            ui.horizontal(|ui| {
                ui.label(format!("Device {}", device.device_id));
                egui::ComboBox::from_id_salt(format!("media-player-{key}"))
                    .selected_text(selected.clone())
                    .show_ui(ui, |ui| {
                        for model in &device.players {
                            ui.selectable_value(&mut selected, model.player.clone(), &model.player);
                        }
                    });
                if ui.button("Refresh").clicked() {
                    commands.push(WorkerCommand::Media {
                        device_id: device.device_id.clone(),
                        command: RemoteMediaCommand::RequestStatus {
                            player: selected.clone(),
                        },
                    });
                }
            });
            let Some(model) = device.players.iter().find(|model| model.player == selected) else {
                continue;
            };
            let title = if model.title.is_empty() {
                &model.player
            } else {
                &model.title
            };
            let artist = if model.artist.is_empty() {
                String::new()
            } else {
                format!(" — {}", model.artist)
            };
            ui.label(format!("{title}{artist}"));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(model.can_go_previous, egui::Button::new("⏮"))
                    .clicked()
                {
                    commands.push(media_action(
                        device.device_id.clone(),
                        selected.clone(),
                        RemoteMediaAction::Previous,
                    ));
                }
                let label = if model.playing { "⏸" } else { "▶" };
                if ui
                    .add_enabled(model.can_play || model.can_pause, egui::Button::new(label))
                    .clicked()
                {
                    commands.push(media_action(
                        device.device_id.clone(),
                        selected.clone(),
                        RemoteMediaAction::PlayPause,
                    ));
                }
                if ui
                    .add_enabled(model.can_go_next, egui::Button::new("⏭"))
                    .clicked()
                {
                    commands.push(media_action(
                        device.device_id.clone(),
                        selected.clone(),
                        RemoteMediaAction::Next,
                    ));
                }
                if ui.button("⏹").clicked() {
                    commands.push(media_action(
                        device.device_id.clone(),
                        selected.clone(),
                        RemoteMediaAction::Stop,
                    ));
                }
                ui.label(if model.playing { "playing" } else { "paused" });
            });
            let volume_key = (key.clone(), selected.clone());
            let mut volume = *self
                .pending_media_volumes
                .get(&volume_key)
                .unwrap_or(&model.volume);
            let response = ui.add(egui::Slider::new(&mut volume, 0..=100).text("Volume"));
            if response.changed() {
                self.pending_media_volumes.insert(volume_key, volume);
            }
            if response.drag_stopped() || (response.changed() && !response.dragged()) {
                commands.push(WorkerCommand::Media {
                    device_id: device.device_id.clone(),
                    command: RemoteMediaCommand::SetVolume {
                        player: selected.clone(),
                        volume,
                    },
                });
            }
            if model.can_seek && model.length_ms > 0 {
                let position_key = (key.clone(), selected.clone());
                let mut position = *self
                    .pending_positions
                    .get(&position_key)
                    .unwrap_or(&model.position_ms);
                let response =
                    ui.add(egui::Slider::new(&mut position, 0..=model.length_ms).text("Position"));
                if response.changed() {
                    self.pending_positions.insert(position_key, position);
                }
                if response.drag_stopped() || (response.changed() && !response.dragged()) {
                    commands.push(WorkerCommand::Media {
                        device_id: device.device_id.clone(),
                        command: RemoteMediaCommand::SetPosition {
                            player: selected.clone(),
                            position_ms: position,
                        },
                    });
                }
            }
            self.selected_players.insert(key, selected);
        }
        for command in commands {
            self.send(command);
        }
    }

    fn transfers(&mut self, ui: &mut egui::Ui) {
        ui.heading("Transfers");
        let Some(status) = running_status(&self.state) else {
            ui.label("Start the daemon to see transfers.");
            return;
        };
        if status.transfers.is_empty() {
            ui.label("No transfers in flight.");
            return;
        }
        egui::Grid::new("transfers")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                ui.label("Device");
                ui.label("Transfer");
                ui.label("Bytes");
                ui.end_row();
                for transfer in &status.transfers {
                    ui.monospace(transfer.device_id.to_string());
                    ui.label(transfer.transfer_id.to_string());
                    ui.label(match transfer.total {
                        Some(total) => format!("{} / {total}", transfer.bytes),
                        None => format!("{} / ?", transfer.bytes),
                    });
                    ui.end_row();
                }
            });
    }
}

impl eframe::App for HandfastApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
        ctx.request_repaint_after(POLL_INTERVAL);
    }
}

fn status_label(state: &DaemonState) -> &'static str {
    match state {
        DaemonState::Unknown => "Connecting",
        DaemonState::Stopped => "Stopped",
        DaemonState::Running(_) => "Running",
        DaemonState::Unreachable(_) => "Unreachable",
    }
}

fn running_status(state: &DaemonState) -> Option<&DaemonStatus> {
    match state {
        DaemonState::Running(status) => Some(status.as_ref()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::control::{
        DeviceSummary, IdentitySummary, PairedDevice, PairingSummary, TransferSummary,
    };
    use handfast_proto::DeviceId;

    fn device_id() -> DeviceId {
        DeviceId::new("0123456789abcdef0123456789abcdef").expect("a valid device id")
    }

    #[test]
    fn the_window_icon_is_a_square_rgba_image() {
        let icon = icon();
        assert_eq!(icon.width, 128);
        assert_eq!(icon.height, 128);
        assert_eq!(icon.rgba.len(), (128 * 128 * 4) as usize);
        assert!(icon
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] > 0));
    }

    // Serializes the process-global `XDG_RUNTIME_DIR` change.
    #[cfg(unix)]
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn sample_status() -> DaemonStatus {
        DaemonStatus {
            version: "0.2.0".to_string(),
            pid: 4242,
            uptime_secs: 17,
            identity: IdentitySummary {
                device_id: device_id(),
                device_name: "Laptop".to_string(),
                device_type: "desktop".to_string(),
                fingerprint: "ab".repeat(32),
                tcp_port: 1716,
            },
            incoming_dir: PathBuf::from("/home/me/Downloads/Handfast"),
            connected: vec![DeviceSummary {
                device_id: device_id(),
                device_name: "Pixel".to_string(),
                device_type: "phone".to_string(),
                connected: true,
            }],
            paired: vec![PairedDevice {
                device_id: device_id(),
                device_name: "Pixel".to_string(),
                device_type: "phone".to_string(),
                connected: true,
            }],
            pending_pairings: vec![PairingSummary {
                device_id: device_id(),
                device_name: "Tablet".to_string(),
                fingerprint: "cd".repeat(32),
            }],
            transfers: vec![TransferSummary {
                device_id: device_id(),
                transfer_id: 7,
                bytes: 1024,
                total: Some(4096),
            }],
            remote_sinks: Vec::new(),
            remote_players: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn a_status_snapshot_becomes_a_running_view() {
        let status = sample_status();
        assert_eq!(
            reduce(Ok(status.clone())),
            DaemonState::Running(Box::new(status))
        );
    }

    #[test]
    fn a_pending_pairing_reaches_the_running_view() {
        let mut status = sample_status();
        status.pending_pairings = vec![PairingSummary {
            device_id: device_id(),
            device_name: "Tablet".to_string(),
            fingerprint: "cd".repeat(32),
        }];
        let DaemonState::Running(status) = reduce(Ok(status)) else {
            panic!("a status snapshot is a running view");
        };
        assert_eq!(status.pending_pairings.len(), 1);
        assert_eq!(status.pending_pairings[0].device_name, "Tablet");
        assert_eq!(status.pending_pairings[0].fingerprint, "cd".repeat(32));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_worker_sends_a_pairing_decision() {
        use crate::control::{ControlResponse, ControlServer, DaemonCommand};
        use tokio::sync::{mpsc, oneshot};

        let _env = ENV_LOCK.lock().await;
        let runtime = tempfile::tempdir().expect("a temp runtime dir");
        std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
        let data_dir = tempfile::tempdir().expect("a temp data dir");
        let row_fingerprint = "cd".repeat(32);
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = ControlServer::start(data_dir.path(), commands)
            .await
            .expect("start the control server");
        let (observed, observed_rx) = oneshot::channel();
        let script = tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                match command {
                    DaemonCommand::Status { reply } => {
                        let _ = reply.send(ControlResponse::Status {
                            status: Box::new(sample_status()),
                        });
                    }
                    DaemonCommand::PairingDecision {
                        device_id,
                        accept,
                        fingerprint,
                        reply,
                    } => {
                        let _ = observed.send((device_id, accept, fingerprint));
                        let _ = reply.send(ControlResponse::Decided);
                        break;
                    }
                    _ => {}
                }
            }
        });

        let (worker, _states) =
            spawn_worker(PathBuf::from("handfast"), data_dir.path().to_path_buf());
        worker
            .send(WorkerCommand::PairingDecision {
                device_id: device_id(),
                accept: true,
                fingerprint: row_fingerprint.clone(),
            })
            .expect("the worker is running");

        let (seen, accept, seen_fingerprint) =
            tokio::time::timeout(Duration::from_secs(5), observed_rx)
                .await
                .expect("the worker sends the decision")
                .expect("the script observes the request");
        assert_eq!(seen, device_id());
        assert!(accept);
        assert_eq!(
            seen_fingerprint, row_fingerprint,
            "the row fingerprint travels"
        );

        drop(worker);
        server.shutdown().await;
        script.await.expect("the script joins");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_worker_sends_a_ring() {
        use crate::control::{ControlResponse, ControlServer, DaemonCommand};
        use tokio::sync::{mpsc, oneshot};

        let _env = ENV_LOCK.lock().await;
        let runtime = tempfile::tempdir().expect("a temp runtime dir");
        std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
        let data_dir = tempfile::tempdir().expect("a temp data dir");
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = ControlServer::start(data_dir.path(), commands)
            .await
            .expect("start the control server");
        let (observed, observed_rx) = oneshot::channel();
        let script = tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                match command {
                    DaemonCommand::Status { reply } => {
                        let _ = reply.send(ControlResponse::Status {
                            status: Box::new(sample_status()),
                        });
                    }
                    DaemonCommand::Ring { device_id, reply } => {
                        let _ = observed.send(device_id);
                        let _ = reply.send(ControlResponse::Ok { bytes: 0 });
                        break;
                    }
                    _ => {}
                }
            }
        });

        let (worker, _states) =
            spawn_worker(PathBuf::from("handfast"), data_dir.path().to_path_buf());
        worker
            .send(WorkerCommand::Ring {
                device_id: device_id(),
            })
            .expect("the worker is running");

        let seen = tokio::time::timeout(Duration::from_secs(5), observed_rx)
            .await
            .expect("the worker sends the ring")
            .expect("the script observes the request");
        assert_eq!(seen, device_id());

        drop(worker);
        server.shutdown().await;
        script.await.expect("the script joins");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_worker_sends_a_remote_volume() {
        use crate::control::{ControlResponse, ControlServer, DaemonCommand};
        use tokio::sync::{mpsc, oneshot};

        let _env = ENV_LOCK.lock().await;
        let runtime = tempfile::tempdir().expect("a temp runtime dir");
        std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
        let data_dir = tempfile::tempdir().expect("a temp data dir");
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = ControlServer::start(data_dir.path(), commands)
            .await
            .expect("start the control server");
        let (observed, observed_rx) = oneshot::channel();
        let script = tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                match command {
                    DaemonCommand::Status { reply } => {
                        let _ = reply.send(ControlResponse::Status {
                            status: Box::new(sample_status()),
                        });
                    }
                    DaemonCommand::RemoteVolume {
                        device_id,
                        sink,
                        percent,
                        muted,
                        reply,
                    } => {
                        let _ = observed.send((device_id, sink, percent, muted));
                        let _ = reply.send(ControlResponse::Ok { bytes: 0 });
                        break;
                    }
                    _ => {}
                }
            }
        });

        let (worker, _states) =
            spawn_worker(PathBuf::from("handfast"), data_dir.path().to_path_buf());
        worker
            .send(WorkerCommand::RemoteVolume {
                device_id: device_id(),
                sink: "speakers".to_string(),
                percent: Some(40),
                muted: None,
            })
            .expect("the worker is running");

        let seen = tokio::time::timeout(Duration::from_secs(5), observed_rx)
            .await
            .expect("the worker sends the volume")
            .expect("the script observes the request");
        assert_eq!(seen.0, device_id());
        assert_eq!(seen.1, "speakers");
        assert_eq!(seen.2, Some(40));
        assert_eq!(seen.3, None);

        drop(worker);
        server.shutdown().await;
        script.await.expect("the script joins");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_worker_sends_a_media_command() {
        use crate::control::{ControlResponse, ControlServer, DaemonCommand};
        use tokio::sync::{mpsc, oneshot};

        let _env = ENV_LOCK.lock().await;
        let runtime = tempfile::tempdir().expect("a temp runtime dir");
        std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
        let data_dir = tempfile::tempdir().expect("a temp data dir");
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = ControlServer::start(data_dir.path(), commands)
            .await
            .expect("start the control server");
        let (observed, observed_rx) = oneshot::channel();
        let script = tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                match command {
                    DaemonCommand::Status { reply } => {
                        let _ = reply.send(ControlResponse::Status {
                            status: Box::new(sample_status()),
                        });
                    }
                    DaemonCommand::Media {
                        device_id,
                        command,
                        reply,
                    } => {
                        let _ = observed.send((device_id, command));
                        let _ = reply.send(ControlResponse::Ok { bytes: 0 });
                        break;
                    }
                    _ => {}
                }
            }
        });

        let (worker, _states) =
            spawn_worker(PathBuf::from("handfast"), data_dir.path().to_path_buf());
        worker
            .send(WorkerCommand::Media {
                device_id: device_id(),
                command: RemoteMediaCommand::Action {
                    player: "mpv".to_string(),
                    action: RemoteMediaAction::Next,
                },
            })
            .expect("the worker is running");

        let seen = tokio::time::timeout(Duration::from_secs(5), observed_rx)
            .await
            .expect("the worker sends the media command")
            .expect("the script observes the request");
        assert_eq!(seen.0, device_id());
        assert_eq!(
            seen.1,
            RemoteMediaCommand::Action {
                player: "mpv".to_string(),
                action: RemoteMediaAction::Next,
            }
        );

        drop(worker);
        server.shutdown().await;
        script.await.expect("the script joins");
    }

    #[test]
    fn a_missing_daemon_becomes_a_stopped_view() {
        assert_eq!(reduce(Err(ControlError::NotRunning)), DaemonState::Stopped);
        assert_eq!(status_label(&DaemonState::Stopped), "Stopped");
    }

    #[test]
    fn any_other_control_error_becomes_an_unreachable_view() {
        let state = reduce(Err(ControlError::Remote("daemon shut down".to_string())));
        assert_eq!(
            state,
            DaemonState::Unreachable("daemon shut down".to_string())
        );
        assert_eq!(status_label(&state), "Unreachable");
        assert!(running_status(&state).is_none());
    }

    #[test]
    fn daemon_command_passes_the_data_dir_and_the_log_file() {
        let data_dir = Path::new("/home/me/.local/share/handfast");
        let command = daemon_command(Path::new("/opt/handfast/handfast"), data_dir);
        assert_eq!(
            command.get_program(),
            std::ffi::OsStr::new("/opt/handfast/handfast")
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "daemon".to_string(),
                "--data-dir".to_string(),
                data_dir.to_string_lossy().into_owned(),
                "--log-file".to_string(),
                data_dir.join(LOG_FILE).to_string_lossy().into_owned(),
            ]
        );
    }

    #[test]
    fn open_command_uses_the_platform_opener_and_the_folder() {
        let path = Path::new("/home/me/Downloads/Handfast");
        let command = open_command(path);
        #[cfg(windows)]
        let expected = "explorer.exe";
        #[cfg(not(windows))]
        let expected = "xdg-open";
        assert_eq!(command.get_program(), std::ffi::OsStr::new(expected));
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec![path.to_string_lossy().into_owned()]);
    }
}
