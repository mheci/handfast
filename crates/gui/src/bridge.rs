//! Subscription bridging handfast-ipc into iced messages.
//!
//! # Threading model
//!
//! Iced owns the main/windowing threads and drives our stream tasks on its
//! own executor, while the handfast-ipc client transport is tokio-based and
//! requires every socket operation to be polled inside a tokio runtime
//! context. To satisfy both, [`Subscription::run`] drives
//! [`ipc_event_stream`], which spawns one dedicated OS thread ("the driver").
//! The driver builds a small *single-threaded* tokio runtime **inside the
//! subscription** and runs the whole connect → initial loads →
//! `select{events, requests}` loop on it, forwarding results to the UI as
//! [`Message`]s through an unbounded channel.
//!
//! Commands travel the opposite way through another unbounded channel handed
//! to the application via [`Message::BridgeReady`]. Each command's request
//! future is awaited directly by the driver — handfast-ipc matches responses
//! back to their request internally, so no correlation ids are kept here.
//!
//! On disconnect the driver waits [`RECONNECT_DELAY`] and retries forever.
//! This reconnect backoff is the only polling-style loop in the GUI and is
//! justified by the thin-client nature of the front-end.
//!
//! GUI speaks only handfast-ipc over UDS; Wayland constraints live in the
//! daemon (see docs/IPC.md).

use std::thread;
use std::time::Duration;

use iced::Subscription;
use iced::futures::{channel::mpsc, sink::SinkExt, Stream, StreamExt};
use handfast_ipc::{default_socket_path, Client, Error, Request, Response};
use tokio::runtime::Builder;
use tokio::time::sleep;

use crate::app::{Bridge, Message};
use crate::model::{self, ConnState};

/// Buffer size of the iced-facing message pump.
const CHANNEL_BUFFER: usize = 100;

/// How long to wait before reconnecting after a lost connection.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Commands the application sends into the IPC driver thread.
#[derive(Debug)]
pub(crate) enum BridgeIn {
    /// Re-fetch the device list from the daemon.
    RefreshDevices,
    /// Fetch the plugin list for one device.
    ListPlugins(String),
    /// Enable or disable one plugin on one device.
    SetPlugin {
        /// Target device identifier.
        device_id: String,
        /// Plugin identifier.
        plugin: String,
        /// Desired enabled state.
        enabled: bool,
    },
    /// Start pairing with a device.
    Pair(String),
    /// Revoke the pairing of a device.
    Unpair(String),
    /// Queue a local file for transfer to a device.
    SendFile {
        /// Target device identifier.
        device_id: String,
        /// Absolute path of the local file.
        path: String,
    },
    /// Dismiss one mirrored notification.
    DismissNotification(String),
    /// Overwrite the daemon's clipboard text.
    ClipboardSet(String),
}

/// The IPC event subscription; keep this alive for the whole session.
pub(crate) fn subscription() -> Subscription<Message> {
    Subscription::run(ipc_event_stream)
}

/// Stream builder handed to [`Subscription::run`]; the function pointer's
/// identity keys the subscription across view rebuilds.
fn ipc_event_stream() -> impl Stream<Item = Message> {
    iced::stream::channel(CHANNEL_BUFFER, async move |mut sender| {
        let (out, mut inbox) = mpsc::unbounded::<Message>();
        match thread::Builder::new()
            .name("handfast-ipc-driver".to_owned())
            .spawn(move || drive(out))
        {
            Ok(_handle) => {
                // Forward driver output into the subscription until the
                // application goes away and drops this stream.
                while let Some(message) = inbox.next().await {
                    if sender.send(message).await.is_err() {
                        break;
                    }
                }
            }
            Err(err) => {
                // Cannot retry meaningfully; surface it and park so the
                // subscription identity stays registered without spinning.
                let _ = sender
                    .send(Message::LoadFailed(format!(
                        "failed to start IPC driver thread: {err}"
                    )))
                    .await;
                std::future::pending::<()>().await;
            }
        }
    })
}

/// Driver-thread entry point: own the private runtime and run all sessions.
fn drive(out: mpsc::UnboundedSender<Message>) {
    let runtime = match Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = out.unbounded_send(Message::LoadFailed(format!(
                "failed to create IPC driver runtime: {err}"
            )));
            return;
        }
    };

    let (commands, mut inbox_commands) = mpsc::unbounded::<BridgeIn>();
    let _ = out.unbounded_send(Message::BridgeReady(Bridge(commands)));

    runtime.block_on(run_sessions(&out, &mut inbox_commands));
}

/// Connect/reconnect loop; each iteration owns one client connection.
async fn run_sessions(
    out: &mpsc::UnboundedSender<Message>,
    commands: &mut mpsc::UnboundedReceiver<BridgeIn>,
) {
    let path = default_socket_path();

    loop {
        notify(out, Message::IpcStatus(ConnState::Connecting));

        let mut client = match Client::connect(&path).await {
            Ok(client) => client,
            Err(err) => {
                notify(out, Message::IpcStatus(ConnState::Disconnected));
                notify(
                    out,
                    Message::ConnectionLost(format!(
                        "daemon unreachable at {}: {err}; retrying every {}s",
                        path.display(),
                        RECONNECT_DELAY.as_secs()
                    )),
                );
                sleep(RECONNECT_DELAY).await;
                continue;
            }
        };

        notify(out, Message::IpcStatus(ConnState::Connected));
        load_snapshot(&client, out).await;

        // The daemon queues a Hello frame before anything else and events
        // stay buffered until the receiver is taken, so claiming it after
        // the snapshot requests above loses nothing.
        let Some(mut events) = client.take_event_receiver() else {
            notify(
                out,
                Message::ConnectionLost("daemon provided no event stream".to_owned()),
            );
            sleep(RECONNECT_DELAY).await;
            continue;
        };

        loop {
            tokio::select! {
                biased;

                event = events.recv() => match event {
                    Some(event) => notify(out, Message::Event(event)),
                    None => break, // reader task exited: connection lost
                },
                command = commands.next() => match command {
                    Some(command) => perform(&client, out, command).await,
                    None => return, // application dropped its sender: shutting down
                },
            }
        }

        notify(out, Message::IpcStatus(ConnState::Disconnected));
        sleep(RECONNECT_DELAY).await;
    }
}

/// Load devices and notifications once per connection.
async fn load_snapshot(client: &Client, out: &mpsc::UnboundedSender<Message>) {
    let devices = unwrap_response(client.request(Request::DeviceList).await);
    notify(out, match devices {
        Ok(payload) => Message::DevicesLoaded(model::parse_devices(&payload)),
        Err(err) => Message::LoadFailed(format!("device list failed: {err}")),
    });

    let notifications = unwrap_response(client.request(Request::NotificationList).await);
    notify(out, match notifications {
        Ok(payload) => Message::NotificationsLoaded(model::parse_notifications(&payload)),
        Err(err) => Message::LoadFailed(format!("notification list failed: {err}")),
    });
}

/// Execute one user-initiated request and report its outcome.
async fn perform(client: &Client, out: &mpsc::UnboundedSender<Message>, command: BridgeIn) {
    match command {
        BridgeIn::RefreshDevices => {
            let reply = unwrap_response(client.request(Request::DeviceList).await);
            notify(out, match reply {
                Ok(payload) => Message::DevicesLoaded(model::parse_devices(&payload)),
                Err(err) => Message::LoadFailed(format!("refresh failed: {err}")),
            });
        }
        BridgeIn::ListPlugins(device_id) => {
            let reply =
                unwrap_response(client.request(Request::PluginList { device_id }).await);
            notify(out, match reply {
                Ok(payload) => Message::PluginsLoaded(model::parse_plugins(&payload)),
                Err(err) => Message::LoadFailed(format!("plugin list failed: {err}")),
            });
        }
        BridgeIn::SetPlugin {
            device_id,
            plugin,
            enabled,
        } => {
            let request = Request::PluginSetEnabled {
                device_id: device_id.clone(),
                plugin: plugin.clone(),
                enabled,
            };
            let outcome = unwrap_response(client.request(request).await).map(|_| ());
            notify(out, Message::PluginSet(outcome));

            // Follow up with the authoritative list so the checkbox reflects
            // what the daemon actually stored.
            let refreshed =
                unwrap_response(client.request(Request::PluginList { device_id }).await);
            if let Ok(payload) = refreshed {
                notify(out, Message::PluginsLoaded(model::parse_plugins(&payload)));
            }
        }
        BridgeIn::Pair(device_id) => {
            let outcome =
                unwrap_response(client.request(Request::DevicePair { device_id }).await)
                    .map(|_| ());
            notify(out, Message::Paired(outcome));
        }
        BridgeIn::Unpair(device_id) => {
            let outcome =
                unwrap_response(client.request(Request::DeviceUnpair { device_id }).await)
                    .map(|_| ());
            notify(out, Message::Unpaired(outcome));
        }
        BridgeIn::SendFile { device_id, path } => {
            let outcome =
                unwrap_response(client.request(Request::SendFile { device_id, path }).await)
                    .map(|_| ());
            notify(out, Message::FileQueued(outcome));
        }
        BridgeIn::DismissNotification(notification_id) => {
            let outcome = unwrap_response(
                client
                    .request(Request::NotificationDismiss { notification_id })
                    .await,
            )
            .map(|_| ());
            notify(out, Message::Dismissed(outcome));
        }
        BridgeIn::ClipboardSet(text) => {
            let outcome =
                unwrap_response(client.request(Request::ClipboardSet { text }).await)
                    .map(|_| ());
            notify(out, Message::ClipboardSet(outcome));
        }
    }
}

/// Fire-and-forget send into the unbounded driver output.
fn notify(out: &mpsc::UnboundedSender<Message>, message: Message) {
    let _ = out.unbounded_send(message);
}

/// Collapse a request outcome into either the response payload or a
/// human-readable failure string.
fn unwrap_response(result: Result<Response, Error>) -> Result<serde_json::Value, String> {
    match result {
        Ok(Response::Ok { result }) => Ok(result),
        Ok(Response::Err { code, message }) => Err(format!("daemon error {code}: {message}")),
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ok_responses_yield_their_payload() {
        let payload = unwrap_response(Ok(Response::ok_json(json!({"a": 1}))));
        assert_eq!(payload, Ok(json!({"a": 1})));
    }

    #[test]
    fn daemon_errors_carry_code_and_message() {
        let result = unwrap_response(Ok(Response::err(7, "denied")));
        assert!(
            matches!(result, Err(ref message)
                if message.contains("7") && message.contains("denied"))
        );
    }

    #[test]
    fn transport_errors_are_stringified() {
        let result = unwrap_response(Err(Error::Closed));
        assert!(matches!(result, Err(ref message) if !message.is_empty()));
    }
}
