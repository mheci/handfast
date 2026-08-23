//! Terminal session lifecycle and the interactive event loop.
//!
//! The loop multiplexes four sources with `tokio::select!`:
//!
//! 1. crossterm's async [`EventStream`] (keyboard and resize input),
//! 2. daemon-pushed [`ServerEvent`]s from the IPC connection,
//! 3. an internal channel of [`Outcome`]s produced by backgrounded requests,
//! 4. a 500 ms tick that exists *only* to flush dirty redraws — all state
//!    changes are event-driven and nothing animates on a timer.
//!
//! Frames are drawn at the top of each iteration whenever [`State::dirty`] is
//! set, so bursts of events coalesce into one draw per loop pass. The terminal
//! is restored on every exit path: by a drop guard around `run` (covers both
//! normal returns and error propagation) and by the process panic hook
//! installed in `main`.

use std::io::{Stdout, Write as _};

use crossterm::{
    cursor::{Hide, Show},
    event::{Event as CrosstermEvent, EventStream},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use handfast_ipc::{Client, ServerEvent};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::{interval, Duration, MissedTickBehavior};

use crate::cmd;
use crate::model::short_hash;
use crate::state::{Action, Outcome, State};
use crate::view;

/// Fully constructed ratatui terminal over stdout.
type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Best-effort terminal restoration; safe to call repeatedly.
///
/// Called from the panic hook (before any panic message is printed) and from
/// [`TerminalGuard`] on every other exit path.
pub(crate) fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(out, LeaveAlternateScreen, Show);
    let _ = out.flush();
}

/// RAII counterpart to [`setup_terminal`]: restoring on drop means every
/// exit path of [`run`] (return, `?`, panic unwind) leaves a usable shell.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Switch the terminal into raw mode + alternate screen and wrap it in a
/// ratatui [`Terminal`].
fn setup_terminal() -> crate::error::Result<TuiTerminal> {
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    crossterm::execute!(out, EnterAlternateScreen, Hide)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

/// Run the interactive interface until the user quits.
///
/// # Errors
/// Terminal setup/teardown and frame-draw failures are returned after the
/// terminal has been restored; daemon-side and IPC failures surface as footer
/// status messages instead of tearing down the session.
pub(crate) async fn run(mut client: Client) -> crate::error::Result<()> {
    let mut terminal = setup_terminal()?;
    let _terminal_guard = TerminalGuard;

    let mut state = State::new();

    let (outcomes_tx, mut outcomes_rx) = mpsc::unbounded_channel::<Outcome>();
    spawn_bootstrap(client.clone(), outcomes_tx.clone());

    let mut server_events = client.take_event_receiver();
    let mut term_input = Some(EventStream::new());

    // The ticker never produces state changes itself; it only wakes the loop
    // so a dirty flag set without further input still reaches the screen.
    let mut ticker = interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    while !state.quit {
        if state.dirty {
            state.dirty = false;
            terminal.draw(|frame| view::draw(frame, &state))?;
        }

        tokio::select! {
            biased;

            event = next_server_event(server_events.as_mut()) => {
                match event {
                    Some(event) => state.apply_event(&event),
                    None => {
                        server_events = None;
                        state.set_flash("daemon disconnected");
                    }
                }
            }
            outcome = outcomes_rx.recv() => {
                if let Some(outcome) = outcome {
                    state.apply_outcome(outcome);
                }
            }
            input = next_input(term_input.as_mut()) => {
                match input {
                    Some(Ok(event)) => {
                        handle_terminal_event(&mut state, event, &client, &outcomes_tx);
                    }
                    Some(Err(error)) => state.set_flash(format!("input error: {error}")),
                    None => term_input = None,
                }
            }
            _ = ticker.tick() => {}
        }
    }

    Ok(())
}

/// Await the next server event; parks forever once the receiver is exhausted
/// so a dead event feed cannot busy-loop the session.
async fn next_server_event(
    receiver: Option<&mut UnboundedReceiver<ServerEvent>>,
) -> Option<ServerEvent> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// Await the next terminal input; parks forever once the stream ends.
async fn next_input(stream: Option<&mut EventStream>) -> Option<std::io::Result<CrosstermEvent>> {
    match stream {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

/// Kick off initial data loads so the first frames show real content.
fn spawn_bootstrap(client: Client, outcomes: UnboundedSender<Outcome>) {
    tokio::spawn(async move {
        match cmd::fetch_devices(&client).await {
            Ok(entries) => {
                let _ = outcomes.send(Outcome::Devices(entries));
            }
            Err(error) => {
                let _ = outcomes.send(Outcome::Flash(format!("devices unavailable: {error}")));
            }
        }
        match cmd::fetch_notifications(&client).await {
            Ok(rows) => {
                let _ = outcomes.send(Outcome::Notifications(rows));
            }
            Err(error) => {
                let _ = outcomes.send(Outcome::Flash(format!(
                    "notifications unavailable: {error}"
                )));
            }
        }
    });
}

/// Route raw crossterm input: keys go through the reducer, resizes force a
/// redraw, everything else is ignored.
fn handle_terminal_event(
    state: &mut State,
    event: CrosstermEvent,
    client: &Client,
    outcomes: &UnboundedSender<Outcome>,
) {
    match event {
        CrosstermEvent::Key(key) => {
            if let Some(action) = state.handle_key(key) {
                perform(state, action, client, outcomes);
            }
        }
        CrosstermEvent::Resize(..) => state.dirty = true,
        _ => {}
    }
}

/// Execute a side-effectful [`Action`], spawning the IPC round-trip so the
/// UI stays responsive while requests are in flight.
fn perform(
    state: &mut State,
    action: Action,
    client: &Client,
    outcomes: &UnboundedSender<Outcome>,
) {
    match action {
        Action::Quit => state.quit = true,

        Action::Pair(device_id) => {
            state.set_flash(format!("pairing {}…", short_hash(&device_id)));
            let client = client.clone();
            let outcomes = outcomes.clone();
            tokio::spawn(async move {
                match cmd::pair(&client, &device_id).await {
                    Ok(()) => {
                        let _ = outcomes
                            .send(Outcome::Flash(format!("pair requested for {device_id}")));
                    }
                    Err(error) => {
                        let _ = outcomes.send(Outcome::Flash(format!("pair failed: {error}")));
                    }
                }
                refresh_devices(&client, &outcomes).await;
            });
        }

        Action::Unpair(device_id) => {
            state.set_flash(format!("unpairing {}…", short_hash(&device_id)));
            let client = client.clone();
            let outcomes = outcomes.clone();
            tokio::spawn(async move {
                match cmd::unpair(&client, &device_id).await {
                    Ok(()) => {
                        let _ = outcomes.send(Outcome::Flash(format!("unpaired {device_id}")));
                    }
                    Err(error) => {
                        let _ = outcomes.send(Outcome::Flash(format!("unpair failed: {error}")));
                    }
                }
                refresh_devices(&client, &outcomes).await;
            });
        }

        Action::LoadPlugins(device_id) => {
            let client = client.clone();
            let outcomes = outcomes.clone();
            tokio::spawn(async move {
                match cmd::fetch_plugins(&client, &device_id).await {
                    Ok(rows) => {
                        let _ = outcomes.send(Outcome::Plugins {
                            device: device_id,
                            plugins: rows,
                        });
                    }
                    Err(error) => {
                        let _ =
                            outcomes.send(Outcome::Flash(format!("plugin list failed: {error}")));
                    }
                }
            });
        }

        Action::TogglePlugin {
            device_id,
            plugin,
            enabled,
        } => {
            apply_optimistic_toggle(state, &plugin, enabled);
            let verb = if enabled { "enabling" } else { "disabling" };
            state.set_flash(format!("{verb} {plugin}…"));

            let client = client.clone();
            let outcomes = outcomes.clone();
            tokio::spawn(async move {
                if let Err(error) =
                    cmd::set_plugin_enabled(&client, &device_id, &plugin, enabled).await
                {
                    let _ = outcomes.send(Outcome::Flash(format!("toggle failed: {error}")));
                }
                // Re-sync from the authoritative source either way.
                if let Ok(rows) = cmd::fetch_plugins(&client, &device_id).await {
                    let _ = outcomes.send(Outcome::Plugins {
                        device: device_id,
                        plugins: rows,
                    });
                }
            });
        }
        Action::ReplyToNotification {
            notification_id: _,
            text: _,
        } => {
            // Reply is dispatched via the bridge; state already cleared the modal.
        }
    }
}

/// Flip a plugin row locally before confirmation arrives, so Space feels
/// instant; the follow-up `Outcome::Plugins` reconciles reality.
fn apply_optimistic_toggle(state: &mut State, plugin: &str, enabled: bool) {
    if let Some(row) = state.plugins.iter_mut().find(|row| row.name == plugin) {
        row.enabled = enabled;
        state.dirty = true;
    }
}

/// Fetch a fresh authoritative device list into the state.
async fn refresh_devices(client: &Client, outcomes: &UnboundedSender<Outcome>) {
    if let Ok(entries) = cmd::fetch_devices(client).await {
        let _ = outcomes.send(Outcome::Devices(entries));
    }
}
