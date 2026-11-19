//! Telephony events from a local ModemManager instance.
//!
//! A desktop with a modem — a laptop with a WWAN card, a PinePhone, a handful
//! of tablets — exposes calls through ModemManager on the system bus. The
//! desktop client treats the modem as just another telephony source and
//! forwards `ringing`, `talking` and `missedCall` events to paired phones.
//!
//! Handfast polls instead of watching. `mmcli -m <modem> --voice-list-calls`
//! is a cheap one-shot query, polling once per plugin tick is well inside the
//! cost of the tick itself, and a poll cannot miss a D-Bus signal that fired
//! before the process started.
//!
//! The parser is deliberately tolerant: `mmcli` output is a human-readable
//! table whose column order has changed between versions, so state is read by
//! field name from each row rather than by position.

use std::io;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::CallEvent;
use crate::traits::Telephony;

/// The program used to query modems.
pub const PROGRAM: &str = "mmcli";

/// Runs one command and returns its standard output.
pub trait Runner: Send + Sync {
    /// Run `program` with `args`.
    ///
    /// # Errors
    /// Fails when the process cannot be started, exits non-zero, or its output
    /// is not UTF-8.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<String>;
}

/// [`Runner`] backed by [`std::process::Command`].
#[derive(Debug, Clone, Copy, Default)]
pub struct StdRunner;

impl Runner for StdRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<String> {
        let output = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "{program} exited with {}",
                output.status
            )));
        }
        String::from_utf8(output.stdout)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "output is not UTF-8"))
    }
}

/// Map a ModemManager call state onto the wire event name.
///
/// ModemManager spells states `ringing-in`, `active`, `held`, `terminated`.
/// The desktop client only forwards the first two, because a terminated call
/// is reported as a cancellation of the previous event rather than a new one.
#[must_use]
pub fn event_for_state(state: &str) -> Option<&'static str> {
    match state.trim().to_ascii_lowercase().as_str() {
        "ringing-in" | "ringing-out" => Some("ringing"),
        "active" | "dialing" | "alerting" => Some("talking"),
        _ => None,
    }
}

/// Read the first `state` field and the `number` field out of `mmcli` output.
///
/// `mmcli` prints one row per call as a comma-separated list of
/// `key: value` pairs, for example:
///
/// ```text
///   /org/freedesktop/ModemManager1/Call/0
///     -------------------------
///     Number: 5551234
///     Direction: incoming
///     State: ringing-in
/// ```
///
/// The parser accepts that shape and the single-line variant, and returns the
/// first call it recognises.
#[must_use]
pub fn parse_calls(text: &str) -> Vec<CallEvent> {
    let mut events = Vec::new();
    let mut number: Option<String> = None;
    let mut state: Option<String> = None;

    let mut flush = |number: &mut Option<String>, state: &mut Option<String>| {
        if let Some(state_value) = state.take() {
            if let Some(event) = event_for_state(&state_value) {
                events.push(CallEvent {
                    event: event.to_string(),
                    phone_number: number.take(),
                    contact_name: None,
                    is_cancel: false,
                });
            }
        }
        number.take();
    };

    for line in text.lines() {
        let line = line.trim();
        // A new call block starts at the object path header.
        if line.starts_with("/org/freedesktop/ModemManager1/Call/") {
            flush(&mut number, &mut state);
            continue;
        }
        // Some versions print every call on one line, comma separated.
        if line.contains("state:") && line.contains(',') {
            for field in line.split(',') {
                let Some((key, value)) = field.split_once(':') else {
                    continue;
                };
                match key.trim().to_ascii_lowercase().as_str() {
                    "number" => number = Some(value.trim().to_string()),
                    "state" => state = Some(value.trim().to_string()),
                    _ => {}
                }
            }
            flush(&mut number, &mut state);
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "number" => {
                let value = value.trim();
                if !value.is_empty() {
                    number = Some(value.to_string());
                }
            }
            "state" => state = Some(value.trim().to_string()),
            _ => {}
        }
    }
    flush(&mut number, &mut state);
    events
}

/// Polls ModemManager through `mmcli`.
pub struct MmTelephony {
    runner: Arc<dyn Runner>,
}

impl MmTelephony {
    /// Build an adapter that runs the real `mmcli`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(StdRunner),
        }
    }

    /// Build an adapter over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn Runner>) -> Self {
        Self { runner }
    }
}

impl Default for MmTelephony {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Telephony for MmTelephony {
    async fn poll(&self) -> Result<Vec<CallEvent>> {
        let runner = Arc::clone(&self.runner);
        let text =
            tokio::task::spawn_blocking(move || runner.run(PROGRAM, &["--voice-list-calls"]))
                .await
                .map_err(|error| DesktopError::Backend(error.to_string()))?
                .map_err(|error| match error.kind() {
                    io::ErrorKind::NotFound => {
                        DesktopError::Unsupported("mmcli is not installed")
                    }
                    _ => DesktopError::Backend(error.to_string()),
                })?;
        Ok(parse_calls(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeRunner {
        output: io::Result<String>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl Runner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<String> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
            ));
            match &self.output {
                Ok(text) => Ok(text.clone()),
                Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
            }
        }
    }

    fn adapter(output: io::Result<String>) -> (MmTelephony, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner {
            output,
            calls: Mutex::new(Vec::new()),
        });
        (
            MmTelephony::with_runner(Arc::clone(&runner) as Arc<dyn Runner>),
            runner,
        )
    }

    #[test]
    fn states_map_to_wire_events() {
        assert_eq!(event_for_state("ringing-in"), Some("ringing"));
        assert_eq!(event_for_state("Ringing-Out"), Some("ringing"));
        assert_eq!(event_for_state("active"), Some("talking"));
        assert_eq!(event_for_state("held"), None);
        // A terminated call is an end, not a new event.
        assert_eq!(event_for_state("terminated"), None);
    }

    #[test]
    fn a_block_form_call_parses() {
        let text = "\
  /org/freedesktop/ModemManager1/Call/0
    -------------------------
    Number: 5551234
    Direction: incoming
    State: ringing-in
";
        let events = parse_calls(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "ringing");
        assert_eq!(events[0].phone_number.as_deref(), Some("5551234"));
    }

    #[test]
    fn a_single_line_call_parses() {
        let text = "number: 5559999, state: active\n";
        let events = parse_calls(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "talking");
        assert_eq!(events[0].phone_number.as_deref(), Some("5559999"));
    }

    #[test]
    fn several_calls_are_reported_in_order() {
        let text = "\
  /org/freedesktop/ModemManager1/Call/0
    Number: 111
    State: ringing-in
  /org/freedesktop/ModemManager1/Call/1
    Number: 222
    State: active
";
        let events = parse_calls(text);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].phone_number.as_deref(), Some("111"));
        assert_eq!(events[1].event, "talking");
    }

    #[test]
    fn a_call_without_a_number_is_still_reported() {
        let text = "State: ringing-in\n";
        let events = parse_calls(text);
        assert_eq!(events.len(), 1);
        assert!(events[0].phone_number.is_none());
    }

    #[test]
    fn non_call_output_yields_nothing() {
        assert!(parse_calls("No voice calls found\n").is_empty());
        assert!(parse_calls("").is_empty());
    }

    #[tokio::test]
    async fn poll_runs_the_voice_listing() {
        let (telephony, runner) = adapter(Ok("State: ringing-in\n".to_string()));
        let events = telephony.poll().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            runner.calls.lock().unwrap().clone(),
            vec![("mmcli".to_string(), vec!["--voice-list-calls".to_string()])]
        );
    }

    #[tokio::test]
    async fn a_missing_tool_is_unsupported() {
        let (telephony, _) =
            adapter(Err(io::Error::new(io::ErrorKind::NotFound, "not found")));
        let error = telephony.poll().await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Unsupported(message) if message.contains("mmcli")),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn a_failing_command_is_a_backend_error() {
        let (telephony, _) = adapter(Err(io::Error::other("modem busy")));
        let error = telephony.poll().await.unwrap_err();
        assert!(matches!(error, DesktopError::Backend(_)));
    }
}
