//! Notification delivery through `notify-send`, closed over D-Bus with `gdbus`.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::{Notification, Urgency};
use crate::traits::Notifier;

const PROGRAM: &str = "notify-send";
const GDBUS: &str = "gdbus";

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

    fn ids(&self) -> MutexGuard<'_, HashMap<String, u32>> {
        self.ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        let args = Self::args(notification);
        let output = self
            .runner
            .run(PROGRAM, &args)
            .await
            .map_err(|error| spawn_error(PROGRAM, error))?;
        if !output.success() {
            return Err(exit_error(PROGRAM, &output));
        }
        let Some(peer_id) = notification.id.as_deref() else {
            return Ok(());
        };
        let backend_id = output.stdout.trim().parse::<u32>().map_err(|_| {
            DesktopError::Backend(format!(
                "{PROGRAM} printed no notification id, got {:?}",
                output.stdout
            ))
        })?;
        self.ids().insert(peer_id.to_string(), backend_id);
        Ok(())
    }

    async fn close(&self, id: &str) -> Result<()> {
        let backend_id = self.ids().get(id).copied().ok_or_else(|| {
            DesktopError::Backend(format!(
                "{PROGRAM} has no backend id for notification {id:?}"
            ))
        })?;
        let output = self
            .runner
            .run(GDBUS, &Self::close_args(backend_id))
            .await
            .map_err(|error| spawn_error(GDBUS, error))?;
        if !output.success() {
            return Err(exit_error(GDBUS, &output));
        }
        self.ids().remove(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
        notify: FakeOutcome,
        gdbus: FakeOutcome,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeRunner {
        fn new(notify: FakeOutcome) -> Arc<Self> {
            Self::with_close(notify, FakeOutcome::success())
        }

        fn with_close(notify: FakeOutcome, gdbus: FakeOutcome) -> Arc<Self> {
            Arc::new(Self {
                notify,
                gdbus,
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(&self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            let outcome = if program == GDBUS {
                &self.gdbus
            } else {
                &self.notify
            };
            match outcome {
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
    async fn close_unknown_id_is_an_error() {
        let (notifier, runner) = notifier(FakeOutcome::printing("7"));
        let error = notifier.close("never-seen").await.unwrap_err();
        match error {
            DesktopError::Backend(message) => {
                assert!(message.contains("never-seen"), "{message}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
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
    async fn unparseable_backend_id_is_an_error() {
        let (notifier, _) = notifier(FakeOutcome::printing("not a number"));
        let error = notifier.notify(&posted(Some("peer-1"))).await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Backend(message) if message.contains("no notification id")),
            "unexpected error: {error:?}"
        );
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
