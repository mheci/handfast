//! Notification delivery through `notify-send`, closed over D-Bus with `gdbus`.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::error::{DesktopError, Result};
use crate::model::{Notification, Urgency};
use crate::traits::Notifier;

const PROGRAM: &str = "notify-send";
const GDBUS: &str = "gdbus";

const MAX_PEER_ID_BYTES: usize = 256;
const MAX_TRACKED_IDS: usize = 1024;
const REPLACE_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// Output captured from a finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code, or `None` when the process ended without one.
    pub code: Option<i32>,
    /// Captured standard output, trimmed.
    pub stdout: String,
    /// Captured standard error, trimmed.
    pub stderr: String,
}

impl CommandOutput {
    /// Whether the command exited successfully.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs an external command.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `program` with `args` and capture its exit status and output.
    ///
    /// # Errors
    /// Fails when the process cannot be spawned.
    async fn run(&self, program: &str, args: &[String]) -> io::Result<CommandOutput>;
}

/// A [`CommandRunner`] backed by [`tokio::process::Command`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioCommandRunner;

#[async_trait]
impl CommandRunner for TokioCommandRunner {
    async fn run(&self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
        let output = tokio::process::Command::new(program)
            .kill_on_drop(true)
            .args(args)
            .output()
            .await?;
        Ok(CommandOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

/// A notifier that delivers notifications with `notify-send` and closes them
/// through the freedesktop notification daemon over D-Bus.
#[derive(Clone)]
pub struct CommandNotifier {
    runner: Arc<dyn CommandRunner>,
    /// Peer notification ID to the ID `notify-send` assigned to the displayed
    /// notification.
    ids: Arc<Mutex<HashMap<String, u32>>>,
}

impl CommandNotifier {
    /// Build a notifier that runs the real `notify-send` and `gdbus` binaries.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioCommandRunner),
            ids: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Build a notifier over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            runner,
            ids: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn args(notification: &Notification) -> Vec<String> {
        vec![
            format!("--app-name={}", notification.app_name),
            format!("--urgency={}", urgency_name(notification.urgency)),
            "--print-id".to_string(),
            "--".to_string(),
            notification.title.clone(),
            notification.text.clone(),
        ]
    }

    fn close_args(id: u32) -> Vec<String> {
        vec![
            "call".to_string(),
            "--session".to_string(),
            "--dest".to_string(),
            "org.freedesktop.Notifications".to_string(),
            "--object-path".to_string(),
            "/org/freedesktop/Notifications".to_string(),
            "--method".to_string(),
            "org.freedesktop.Notifications.CloseNotification".to_string(),
            id.to_string(),
        ]
    }

    async fn close_backend(&self, id: u32) -> Result<()> {
        let output = self
            .runner
            .run(GDBUS, &Self::close_args(id))
            .await
            .map_err(|error| spawn_error(GDBUS, error))?;
        if !output.success() {
            return Err(exit_error(GDBUS, &output));
        }
        Ok(())
    }
}

impl Default for CommandNotifier {
    fn default() -> Self {
        Self::new()
    }
}

fn urgency_name(urgency: Urgency) -> &'static str {
    match urgency {
        Urgency::Low => "low",
        Urgency::Normal => "normal",
        Urgency::Critical => "critical",
    }
}

fn spawn_error(program: &'static str, error: io::Error) -> DesktopError {
    if error.kind() == io::ErrorKind::NotFound {
        DesktopError::Unsupported(program)
    } else {
        DesktopError::Backend(format!("{program}: {error}"))
    }
}

fn exit_error(program: &str, output: &CommandOutput) -> DesktopError {
    let status = match output.code {
        Some(code) => format!("exit code {code}"),
        None => "terminated by a signal".to_string(),
    };
    let detail = if output.stderr.is_empty() {
        status
    } else {
        format!("{status}: {}", output.stderr)
    };
    DesktopError::Backend(format!("{program} failed with {detail}"))
}

#[async_trait]
impl Notifier for CommandNotifier {
    async fn notify(&self, notification: &Notification) -> Result<()> {
        let peer_id = notification.id.as_deref();
        if peer_id.is_some_and(|id| id.len() > MAX_PEER_ID_BYTES) {
            return Err(DesktopError::Backend(format!(
                "notification id exceeds {MAX_PEER_ID_BYTES} bytes"
            )));
        }
        let args = Self::args(notification);
        let output = self
            .runner
            .run(PROGRAM, &args)
            .await
            .map_err(|error| spawn_error(PROGRAM, error))?;
        if !output.success() {
            return Err(exit_error(PROGRAM, &output));
        }
        let Some(peer_id) = peer_id else {
            return Ok(());
        };
        let backend_id = output.stdout.trim().parse::<u32>().ok();
        // Store the new id before closing the old one, so a cancelled close
        // cannot leave the displayed notification untracked.
        let previous = {
            let mut ids = self.ids.lock().await;
            let previous = ids.remove(peer_id);
            if let Some(backend_id) = backend_id {
                if ids.len() >= MAX_TRACKED_IDS {
                    ids.clear();
                    eprintln!(
                        "handfast-desktop: notification id map reached {MAX_TRACKED_IDS} entries; clearing it"
                    );
                }
                ids.insert(peer_id.to_string(), backend_id);
            }
            previous
        };
        if let Some(previous) = previous {
            // A stale notification must not fail the new one, and a hung close
            // must not stall other devices.
            let _ = tokio::time::timeout(REPLACE_CLOSE_TIMEOUT, self.close_backend(previous)).await;
        }
        Ok(())
    }

    async fn close(&self, id: &str) -> Result<()> {
        let mut ids = self.ids.lock().await;
        let Some(backend_id) = ids.get(id).copied() else {
            return Ok(());
        };
        self.close_backend(backend_id).await?;
        ids.remove(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;

    #[derive(Debug, Clone)]
    enum FakeOutcome {
        Success { stdout: String },
        Exit { code: i32, stderr: String },
        Spawn(io::ErrorKind),
    }

    impl FakeOutcome {
        fn success() -> Self {
            Self::Success {
                stdout: String::new(),
            }
        }

        fn printing(stdout: &str) -> Self {
            Self::Success {
                stdout: stdout.to_string(),
            }
        }
    }

    struct FakeRunner {
        notify: Mutex<FakeOutcome>,
        gdbus: FakeOutcome,
        calls: Mutex<Vec<(String, Vec<String>)>>,
        gdbus_gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    impl FakeRunner {
        fn new(notify: FakeOutcome) -> Arc<Self> {
            Self::with_close(notify, FakeOutcome::success())
        }

        fn with_close(notify: FakeOutcome, gdbus: FakeOutcome) -> Arc<Self> {
            Arc::new(Self {
                notify: Mutex::new(notify),
                gdbus,
                calls: Mutex::new(Vec::new()),
                gdbus_gate: Mutex::new(None),
            })
        }

        fn set_notify(&self, outcome: FakeOutcome) {
            *self.notify.lock().unwrap() = outcome;
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }

        fn gate_next_gdbus(&self, gate: tokio::sync::oneshot::Receiver<()>) {
            *self.gdbus_gate.lock().unwrap() = Some(gate);
        }

        async fn wait_for_calls(&self, count: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.calls().len() < count {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the fake runner did not reach the expected call");
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(&self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            let gate = if program == GDBUS {
                self.gdbus_gate.lock().unwrap().take()
            } else {
                None
            };
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            let outcome = if program == GDBUS {
                self.gdbus.clone()
            } else {
                self.notify.lock().unwrap().clone()
            };
            match &outcome {
                FakeOutcome::Success { stdout } => Ok(CommandOutput {
                    code: Some(0),
                    stdout: stdout.clone(),
                    stderr: String::new(),
                }),
                FakeOutcome::Exit { code, stderr } => Ok(CommandOutput {
                    code: Some(*code),
                    stdout: String::new(),
                    stderr: stderr.clone(),
                }),
                FakeOutcome::Spawn(kind) => Err(io::Error::new(*kind, "no such program")),
            }
        }
    }

    fn notifier(outcome: FakeOutcome) -> (CommandNotifier, Arc<FakeRunner>) {
        let runner = FakeRunner::new(outcome);
        let backend = Arc::clone(&runner) as Arc<dyn CommandRunner>;
        (CommandNotifier::with_runner(backend), runner)
    }

    fn notifier_closing_with(
        notify: FakeOutcome,
        gdbus: FakeOutcome,
    ) -> (CommandNotifier, Arc<FakeRunner>) {
        let runner = FakeRunner::with_close(notify, gdbus);
        let backend = Arc::clone(&runner) as Arc<dyn CommandRunner>;
        (CommandNotifier::with_runner(backend), runner)
    }

    fn posted(id: Option<&str>) -> Notification {
        let mut notification = Notification::new("Phone", "Message", "Hello");
        notification.id = id.map(ToString::to_string);
        notification
    }

    #[tokio::test]
    async fn notification_passes_app_name_title_and_body() {
        let (notifier, runner) = notifier(FakeOutcome::success());
        notifier
            .notify(&Notification::new("Phone", "Message", "Hello"))
            .await
            .unwrap();
        assert_eq!(
            runner.calls(),
            vec![(
                "notify-send".to_string(),
                vec![
                    "--app-name=Phone".to_string(),
                    "--urgency=normal".to_string(),
                    "--print-id".to_string(),
                    "--".to_string(),
                    "Message".to_string(),
                    "Hello".to_string(),
                ],
            )]
        );
    }

    #[tokio::test]
    async fn urgency_is_mapped() {
        let (notifier, runner) = notifier(FakeOutcome::success());
        for (urgency, expected) in [
            (Urgency::Low, "--urgency=low"),
            (Urgency::Normal, "--urgency=normal"),
            (Urgency::Critical, "--urgency=critical"),
        ] {
            let mut notification = Notification::new("Phone", "Title", "Body");
            notification.urgency = urgency;
            notifier.notify(&notification).await.unwrap();
            let args = runner.calls().last().unwrap().1.clone();
            assert!(
                args.contains(&expected.to_string()),
                "{urgency:?} must map to {expected}, got {args:?}"
            );
        }
    }

    #[tokio::test]
    async fn missing_tool_is_a_clear_error() {
        let (notifier, _) = notifier(FakeOutcome::Spawn(io::ErrorKind::NotFound));
        let error = notifier
            .notify(&Notification::new("App", "Title", "Body"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, DesktopError::Unsupported("notify-send")),
            "unexpected error: {error:?}"
        );
    }

    #[tokio::test]
    async fn non_zero_exit_is_an_error() {
        let (notifier, _) = notifier(FakeOutcome::Exit {
            code: 3,
            stderr: "boom".to_string(),
        });
        let error = notifier
            .notify(&Notification::new("App", "Title", "Body"))
            .await
            .unwrap_err();
        match error {
            DesktopError::Backend(message) => {
                assert!(message.contains("exit code 3"), "{message}");
                assert!(message.contains("boom"), "{message}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn notify_captures_the_backend_id() {
        let (notifier, runner) = notifier(FakeOutcome::printing("42\n"));
        notifier.notify(&posted(Some("peer-1"))).await.unwrap();
        notifier.close("peer-1").await.unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 2, "expected notify-send then gdbus");
        assert_eq!(calls[1].0, "gdbus");
        assert_eq!(calls[1].1.last().map(String::as_str), Some("42"));
    }

    #[tokio::test]
    async fn close_calls_gdbus_with_the_captured_id() {
        let (notifier, runner) = notifier(FakeOutcome::printing("7"));
        notifier.notify(&posted(Some("peer-1"))).await.unwrap();
        notifier.close("peer-1").await.unwrap();
        assert_eq!(
            runner.calls()[1],
            (
                "gdbus".to_string(),
                vec![
                    "call".to_string(),
                    "--session".to_string(),
                    "--dest".to_string(),
                    "org.freedesktop.Notifications".to_string(),
                    "--object-path".to_string(),
                    "/org/freedesktop/Notifications".to_string(),
                    "--method".to_string(),
                    "org.freedesktop.Notifications.CloseNotification".to_string(),
                    "7".to_string(),
                ],
            )
        );
    }

    #[tokio::test]
    async fn same_key_notify_closes_the_old_backend_id() {
        let (notifier, runner) = notifier(FakeOutcome::printing("1"));
        notifier.notify(&posted(Some("peer-1"))).await.unwrap();
        runner.set_notify(FakeOutcome::printing("2"));
        notifier.notify(&posted(Some("peer-1"))).await.unwrap();
        notifier.close("peer-1").await.unwrap();
        let calls = runner.calls();
        assert_eq!(
            calls.len(),
            4,
            "expected two posts, the old close and the new close"
        );
        assert_eq!(calls[2].0, "gdbus");
        assert_eq!(calls[2].1.last().map(String::as_str), Some("1"));
        assert_eq!(calls[3].0, "gdbus");
        assert_eq!(calls[3].1.last().map(String::as_str), Some("2"));
    }

    #[tokio::test]
    async fn close_racing_a_same_key_notify_keeps_the_new_id() {
        let (notifier, runner) = notifier(FakeOutcome::printing("1"));
        notifier.notify(&posted(Some("peer-1"))).await.unwrap();
        runner.set_notify(FakeOutcome::printing("2"));

        let (release, gate) = tokio::sync::oneshot::channel();
        runner.gate_next_gdbus(gate);
        let closing = tokio::spawn({
            let notifier = notifier.clone();
            async move { notifier.close("peer-1").await }
        });
        runner.wait_for_calls(2).await;

        let replacing = tokio::spawn({
            let notifier = notifier.clone();
            async move { notifier.notify(&posted(Some("peer-1"))).await }
        });
        runner.wait_for_calls(3).await;
        release.send(()).unwrap();
        closing.await.unwrap().unwrap();
        replacing.await.unwrap().unwrap();

        notifier.close("peer-1").await.unwrap();

        let closed: Vec<String> = runner
            .calls()
            .into_iter()
            .filter(|(program, _)| program == GDBUS)
            .map(|(_, args)| args.last().unwrap().clone())
            .collect();
        assert_eq!(
            closed.first().map(String::as_str),
            Some("1"),
            "the racing close must close the old backend id"
        );
        assert_eq!(
            closed.last().map(String::as_str),
            Some("2"),
            "the replacement must stay closable after the racing close"
        );
    }

    #[tokio::test]
    async fn close_unknown_id_is_a_no_op() {
        let (notifier, runner) = notifier(FakeOutcome::printing("7"));
        notifier.close("never-seen").await.unwrap();
        assert!(
            runner.calls().is_empty(),
            "close must not spawn a process for an unknown id"
        );
    }

    #[tokio::test]
    async fn close_gdbus_failure_is_an_error() {
        let (notifier, _) = notifier_closing_with(
            FakeOutcome::printing("7"),
            FakeOutcome::Exit {
                code: 1,
                stderr: "no such name".to_string(),
            },
        );
        notifier.notify(&posted(Some("peer-1"))).await.unwrap();
        let error = notifier.close("peer-1").await.unwrap_err();
        match error {
            DesktopError::Backend(message) => {
                assert!(message.contains("gdbus"), "{message}");
                assert!(message.contains("no such name"), "{message}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn notify_without_a_peer_id_needs_no_printed_id() {
        let (notifier, _) = notifier(FakeOutcome::printing(""));
        notifier.notify(&posted(None)).await.unwrap();
    }

    #[tokio::test]
    async fn unparseable_backend_id_is_not_stored() {
        let (notifier, runner) = notifier(FakeOutcome::printing("not a number"));
        notifier.notify(&posted(Some("peer-1"))).await.unwrap();
        notifier.close("peer-1").await.unwrap();
        assert_eq!(
            runner.calls().len(),
            1,
            "close must not spawn when no id was stored"
        );
    }

    #[tokio::test]
    async fn overlong_peer_id_is_rejected() {
        let (notifier, runner) = notifier(FakeOutcome::printing("7"));
        let id = "x".repeat(MAX_PEER_ID_BYTES + 1);
        let error = notifier
            .notify(&posted(Some(id.as_str())))
            .await
            .unwrap_err();
        assert!(
            matches!(error, DesktopError::Backend(_)),
            "unexpected error: {error:?}"
        );
        assert!(
            runner.calls().is_empty(),
            "a rejected id must not spawn notify-send"
        );
    }

    #[tokio::test]
    async fn peer_id_at_the_length_cap_is_tracked() {
        let (notifier, runner) = notifier(FakeOutcome::printing("7"));
        let id = "x".repeat(MAX_PEER_ID_BYTES);
        notifier.notify(&posted(Some(id.as_str()))).await.unwrap();
        notifier.close(&id).await.unwrap();
        assert_eq!(runner.calls().len(), 2, "expected notify-send then gdbus");
    }

    #[tokio::test]
    async fn the_id_map_is_bounded() {
        let (notifier, runner) = notifier(FakeOutcome::printing("5"));
        for index in 0..MAX_TRACKED_IDS {
            let id = format!("peer-{index}");
            notifier.notify(&posted(Some(id.as_str()))).await.unwrap();
        }
        assert_eq!(runner.calls().len(), MAX_TRACKED_IDS);

        notifier.close("peer-0").await.unwrap();
        assert_eq!(
            runner.calls().len(),
            MAX_TRACKED_IDS + 1,
            "an id at the cap is still tracked"
        );

        notifier.notify(&posted(Some("peer-0"))).await.unwrap();
        notifier
            .notify(&posted(Some("peer-overflow")))
            .await
            .unwrap();
        assert_eq!(runner.calls().len(), MAX_TRACKED_IDS + 3);

        notifier.close("peer-1").await.unwrap();
        assert_eq!(
            runner.calls().len(),
            MAX_TRACKED_IDS + 3,
            "the overflow must clear the older ids"
        );
        notifier.close("peer-overflow").await.unwrap();
        assert_eq!(
            runner.calls().len(),
            MAX_TRACKED_IDS + 4,
            "the overflow id must stay closable"
        );
    }

    #[tokio::test]
    async fn a_hung_previous_close_times_out_without_losing_the_new_id() {
        let (notifier, runner) = notifier(FakeOutcome::printing("1"));
        notifier.notify(&posted(Some("peer-1"))).await.unwrap();
        runner.set_notify(FakeOutcome::printing("2"));

        let (release, gate) = tokio::sync::oneshot::channel();
        runner.gate_next_gdbus(gate);
        let replacement = posted(Some("peer-1"));
        tokio::time::timeout(Duration::from_secs(2), notifier.notify(&replacement))
            .await
            .expect("the replacement must not wait on a hung close")
            .unwrap();

        notifier.close("peer-1").await.unwrap();
        let closed: Vec<String> = runner
            .calls()
            .into_iter()
            .filter(|(program, _)| program == GDBUS)
            .map(|(_, args)| args.last().unwrap().clone())
            .collect();
        assert_eq!(
            closed.last().map(String::as_str),
            Some("2"),
            "the replacement must stay closable after the old close times out"
        );
        drop(release);
    }

    #[tokio::test]
    async fn a_cancelled_previous_close_keeps_the_new_id_closable() {
        let (notifier, runner) = notifier(FakeOutcome::printing("1"));
        notifier.notify(&posted(Some("peer-1"))).await.unwrap();
        runner.set_notify(FakeOutcome::printing("2"));

        let (release, gate) = tokio::sync::oneshot::channel();
        runner.gate_next_gdbus(gate);
        let replacing = tokio::spawn({
            let notifier = notifier.clone();
            async move { notifier.notify(&posted(Some("peer-1"))).await }
        });
        runner.wait_for_calls(3).await;
        replacing.abort();
        assert!(replacing.await.unwrap_err().is_cancelled());

        notifier.close("peer-1").await.unwrap();
        let closed: Vec<String> = runner
            .calls()
            .into_iter()
            .filter(|(program, _)| program == GDBUS)
            .map(|(_, args)| args.last().unwrap().clone())
            .collect();
        assert_eq!(
            closed.last().map(String::as_str),
            Some("2"),
            "a cancelled close of the old id must not lose the replacement"
        );
        drop(release);
    }

    #[tokio::test]
    async fn every_command_stays_on_the_fake_runner() {
        let (notifier, runner) = notifier(FakeOutcome::printing("9"));
        notifier.notify(&posted(Some("peer-1"))).await.unwrap();
        notifier.close("peer-1").await.unwrap();
        let programs: Vec<String> = runner
            .calls()
            .into_iter()
            .map(|(program, _)| program)
            .collect();
        assert_eq!(programs, ["notify-send", "gdbus"]);
    }

    #[test]
    fn desktop_accepts_the_command_notifier() {
        let desktop = crate::Desktop::default().with_notifier(crate::backends::notifier());
        assert!(desktop.capabilities().notifications);
    }
}
