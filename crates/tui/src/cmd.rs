//! One-shot subcommand implementations.
//!
//! Split into two layers: thin typed request helpers (`fetch_*`, plain verbs)
//! that the interactive interface reuses from background tasks, and
//! `print_*`/dispatch wrappers rendering tables and summaries for the CLI
//! surface. All output goes to stdout through `std::io::Write`; write failures
//! propagate as [`Error::Io`].

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use handfast_ipc::{Client, Request, ServerEvent};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::cli::{ClipboardAction, NotificationAction, PluginAction};
use crate::error::{Error, Result, expect_ok};
use crate::model::{
    DeviceEntry, NotifRow, PluginRow, extract_text, field_str, field_u64, parse_devices,
    parse_notifications, parse_plugins, yes_no,
};

// ---- connection ---------------------------------------------------------

/// Open a connection to the daemon socket.
pub(crate) async fn connect(socket: &Path) -> Result<Client> {
    tracing::debug!(target: "handfast::tui", socket = %socket.display(), "connecting");
    Ok(Client::connect(socket).await?)
}

// ---- typed request layer (shared with the TUI) ----------------------------

/// Fetch and defensively parse the device list.
pub(crate) async fn fetch_devices(client: &Client) -> Result<Vec<DeviceEntry>> {
    let payload = expect_ok(client.request(Request::DeviceList).await?)?;
    Ok(parse_devices(&payload))
}

/// Fetch and defensively parse the mirrored notification list.
pub(crate) async fn fetch_notifications(client: &Client) -> Result<Vec<NotifRow>> {
    let payload = expect_ok(client.request(Request::NotificationList).await?)?;
    Ok(parse_notifications(&payload))
}

/// Fetch and defensively parse one device's plugin rows.
pub(crate) async fn fetch_plugins(
    client: &Client,
    device_id: &str,
) -> Result<Vec<PluginRow>> {
    let payload = expect_ok(
        client
            .request(Request::PluginList { device_id: device_id.to_owned() })
            .await?,
    )?;
    Ok(parse_plugins(&payload))
}

/// Request pairing with a device.
pub(crate) async fn pair(client: &Client, device_id: &str) -> Result<()> {
    expect_ok(
        client
            .request(Request::DevicePair { device_id: device_id.to_owned() })
            .await?,
    )?;
    Ok(())
}

/// Revoke pairing with a device.
pub(crate) async fn unpair(client: &Client, device_id: &str) -> Result<()> {
    expect_ok(
        client
            .request(Request::DeviceUnpair { device_id: device_id.to_owned() })
            .await?,
    )?;
    Ok(())
}

/// Enable or disable one plugin on a device.
pub(crate) async fn set_plugin_enabled(
    client: &Client,
    device_id: &str,
    plugin: &str,
    enabled: bool,
) -> Result<()> {
    expect_ok(
        client
            .request(Request::PluginSetEnabled {
                device_id: device_id.to_owned(),
                plugin: plugin.to_owned(),
                enabled,
            })
            .await?,
    )?;
    Ok(())
}

/// Queue a file transfer; returns whatever bookkeeping payload the daemon
/// attached (transfer id etc.).
pub(crate) async fn send_file(
    client: &Client,
    device_id: &str,
    file_path: &Path,
) -> Result<Value> {
    let payload = expect_ok(
        client
            .request(Request::SendFile {
                device_id: device_id.to_owned(),
                path: file_path.display().to_string(),
            })
            .await?,
    )?;
    Ok(payload)
}

/// Dismiss one mirrored notification by id.
pub(crate) async fn dismiss_notification(
    client: &Client,
    notification_id: &str,
) -> Result<()> {
    expect_ok(
        client
            .request(Request::NotificationDismiss {
                notification_id: notification_id.to_owned(),
            })
            .await?,
    )?;
    Ok(())
}

/// Read the clipboard text (bare string or `{"text": …}` payloads accepted).
pub(crate) async fn clipboard_text(client: &Client) -> Result<String> {
    let payload = expect_ok(client.request(Request::ClipboardGet).await?)?;
    Ok(extract_text(&payload))
}

/// Overwrite the clipboard text.
pub(crate) async fn store_clipboard(client: &Client, text: &str) -> Result<()> {
    expect_ok(
        client
            .request(Request::ClipboardSet { text: text.to_owned() })
            .await?,
    )?;
    Ok(())
}

// ---- CLI printing layer ---------------------------------------------------

/// `hfctl status`: daemon summary plus ping round-trip time.
///
/// Identity fields are probed defensively under several likely keys; anything
/// the daemon does not report falls back to placeholders instead of failing.
pub(crate) async fn print_status(client: &mut Client, socket: &Path) -> Result<()> {
    let mut events = client.take_event_receiver();

    let info = expect_ok(client.request(Request::DaemonInfo).await?)?;
    let ping_started = Instant::now();
    expect_ok(client.request(Request::Ping).await?)?;
    let ping = ping_started.elapsed();

    let fields = info.as_object();
    let name = fields
        .and_then(|obj| field_str(obj, &["name", "app", "hostname"]))
        .unwrap_or("handfast");
    let version =
        fields.and_then(|obj| field_str(obj, &["version"])).unwrap_or("unknown");
    let pid_label = fields
        .and_then(|obj| field_u64(obj, &["pid"]))
        .map_or_else(|| "pid unknown".to_owned(), |pid| format!("pid {pid}"));

    let mut out = std::io::stdout();
    writeln!(out, "socket:    {}", socket.display())?;
    match drain_hello(events.as_mut()) {
        Some(handshake) => writeln!(out, "handshake: {handshake}")?,
        None => writeln!(out, "handshake: <no Hello received>")?,
    }
    writeln!(out, "daemon:    {name} {version} · {pid_label}")?;
    writeln!(out, "ping:      {:.1} ms", ping.as_secs_f64() * 1000.0)?;
    writeln!(out, "info:      {info}")?;
    Ok(())
}

/// Scan buffered server events for the initial `Hello` handshake.
fn drain_hello(receiver: Option<&mut UnboundedReceiver<ServerEvent>>) -> Option<String> {
    let receiver = receiver?;
    while let Ok(event) = receiver.try_recv() {
        if let ServerEvent::Hello { version, app, pid } = event {
            return Some(format!("{app} · pid {pid} · protocol v{version}"));
        }
    }
    None
}

/// `hfctl devices`: id/name/type/paired/state table.
pub(crate) async fn print_devices(client: &Client) -> Result<()> {
    let entries = fetch_devices(client).await?;
    let rows = entries
        .iter()
        .map(|entry| {
            vec![
                entry.id.clone(),
                entry.name.clone(),
                entry.kind.clone(),
                yes_no(entry.paired).to_owned(),
                entry.state.clone(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["ID", "NAME", "TYPE", "PAIRED", "STATE"], &rows)
}

/// `hfctl pair <DEVICE_ID>`.
pub(crate) async fn print_pair(client: &Client, device_id: &str) -> Result<()> {
    pair(client, device_id).await?;
    println!("pair requested for {device_id}");
    Ok(())
}

/// `hfctl unpair <DEVICE_ID>`.
pub(crate) async fn print_unpair(client: &Client, device_id: &str) -> Result<()> {
    unpair(client, device_id).await?;
    println!("unpaired {device_id}");
    Ok(())
}

/// Dispatch `hfctl plugins …`.
pub(crate) async fn print_plugins_action(
    client: &Client,
    action: PluginAction,
) -> Result<()> {
    match action {
        PluginAction::List { device_id } => {
            let rows = fetch_plugins(client, &device_id).await?;
            let table = rows
                .iter()
                .map(|row| {
                    vec![
                        row.name.clone(),
                        row.title.clone(),
                        yes_no(row.enabled).to_owned(),
                    ]
                })
                .collect::<Vec<_>>();
            print_table(&["PLUGIN", "TITLE", "ENABLED"], &table)
        }
        PluginAction::Enable { device_id, plugin } => {
            set_plugin_enabled(client, &device_id, &plugin, true).await?;
            println!("{plugin} enabled on {device_id}");
            Ok(())
        }
        PluginAction::Disable { device_id, plugin } => {
            set_plugin_enabled(client, &device_id, &plugin, false).await?;
            println!("{plugin} disabled on {device_id}");
            Ok(())
        }
    }
}

/// `hfctl send <DEVICE_ID> <FILE_PATH>`; pretty-prints any transfer payload.
pub(crate) async fn print_send(
    client: &Client,
    device_id: &str,
    file_path: &Path,
) -> Result<()> {
    let payload = send_file(client, device_id, file_path).await?;
    let mut out = std::io::stdout();
    match serde_json::to_string_pretty(&payload) {
        Ok(text) if text != "null" => writeln!(out, "{text}")?,
        _ => writeln!(out, "transfer queued")?,
    }
    Ok(())
}

/// Dispatch `hfctl notifications …`.
pub(crate) async fn print_notifications_action(
    client: &Client,
    action: NotificationAction,
) -> Result<()> {
    match action {
        NotificationAction::List => {
            let rows = fetch_notifications(client).await?;
            let table = rows
                .iter()
                .map(|row| {
                    vec![
                        row.id.clone(),
                        row.app.clone(),
                        row.title.clone(),
                        row.body.clone(),
                    ]
                })
                .collect::<Vec<_>>();
            print_table(&["ID", "APP", "TITLE", "BODY"], &table)
        }
        NotificationAction::Dismiss { notification_id } => {
            dismiss_notification(client, &notification_id).await?;
            println!("dismissed {notification_id}");
            Ok(())
        }
    }
}

/// Dispatch `hfctl clipboard …`.
pub(crate) async fn print_clipboard_action(
    client: &Client,
    action: ClipboardAction,
) -> Result<()> {
    match action {
        ClipboardAction::Get => {
            let text = clipboard_text(client).await?;
            println!("{text}");
        }
        ClipboardAction::Set { text } => {
            let chars = text.chars().count();
            store_clipboard(client, &text).await?;
            println!("clipboard updated ({chars} chars)");
        }
    }
    Ok(())
}

/// `hfctl logs [-n N]`: passive two-second listen window.
///
/// Behavior (mirrored in the CLI help): after connecting, hfctl waits on the
/// daemon's pushed events for a fixed two-second window, collects every
/// [`ServerEvent::LogRecord`] seen during it, prints the trailing `limit`
/// records and exits. No request is sent beyond opening the connection.
pub(crate) async fn print_logs(client: &mut Client, limit: usize) -> Result<()> {
    let mut events = client
        .take_event_receiver()
        .ok_or(Error::EventStreamUnavailable)?;

    const WINDOW: Duration = Duration::from_secs(2);
    let deadline = Instant::now() + WINDOW;
    let mut records: Vec<String> = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(ServerEvent::LogRecord { level, msg })) => {
                records.push(format!("[{level:<5}] {msg}"));
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,        // daemon closed the stream early
            Err(_window_elapsed) => break, // listen window over
        }
    }

    let mut out = std::io::stdout();
    if records.is_empty() {
        writeln!(
            out,
            "(no log records arrived within the {} listen window)",
            WINDOW.as_secs()
        )?;
        return Ok(());
    }
    let skip = records.len().saturating_sub(limit);
    for record in records.iter().skip(skip) {
        writeln!(out, "{record}")?;
    }
    Ok(())
}

/// Render a padded plaintext table. Rows shorter than `headers` are padded
/// with empty cells; nothing panics on ragged input.
pub(crate) fn print_table(headers: &[&str], rows: &[Vec<String>]) -> Result<()> {
    let columns = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|cell| cell.chars().count()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(columns) {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let mut out = std::io::stdout();
    let header_line = headers
        .iter()
        .enumerate()
        .map(|(index, cell)| format!("{cell:<width$}", width = widths[index]))
        .collect::<Vec<_>>()
        .join("  ");
    writeln!(out, "{header_line}")?;

    let rule = widths.iter().sum::<usize>() + 2 * columns.saturating_sub(1);
    writeln!(out, "{}", "-".repeat(rule))?;

    for row in rows {
        let line = (0..columns)
            .map(|index| {
                let cell = row.get(index).map(String::as_str).unwrap_or_default();
                format!("{cell:<width$}", width = widths[index])
            })
            .collect::<Vec<_>>()
            .join("  ");
        writeln!(out, "{line}")?;
    }
    Ok(())
}
