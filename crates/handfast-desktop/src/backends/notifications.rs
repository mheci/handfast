//! Notification delivery through `notify-send`.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::{Notification, Urgency};
use crate::traits::Notifier;

const PROGRAM: &str = "notify-send";

/// Output captured from a finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code, or `None` when the process ended without one.
    pub code: Option<i32>,
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
    /// Run `program` with `args` and capture its exit status and standard error.
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
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

/// A notifier that delivers notifications with `notify-send`.
#[derive(Clone)]
pub struct CommandNotifier {
    runner: Arc<dyn CommandRunner>,
}

impl CommandNotifier {
    /// Build a notifier that runs the real `notify-send` binary.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioCommandRunner),
        }
    }

    /// Build a notifier over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    fn args(notification: &Notification) -> Vec<String> {
        vec![
            format!("--app-name={}", notification.app_name),
            format!("--urgency={}", urgency_name(notification.urgency)),
            "--".to_string(),
            notification.title.clone(),
            notification.text.clone(),
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

#[async_trait]
impl Notifier for CommandNotifier {
    async fn notify(&self, notification: &Notification) -> Result<()> {
        let args = Self::args(notification);
        let output = self.runner.run(PROGRAM, &args).await.map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                DesktopError::Unsupported(PROGRAM)
            } else {
                DesktopError::Backend(format!("{PROGRAM}: {error}"))
            }
        })?;
        if output.success() {
            return Ok(());
        }
        let status = match output.code {
            Some(code) => format!("exit code {code}"),
            None => "terminated by a signal".to_string(),
        };
        let detail = if output.stderr.is_empty() {
            status
        } else {
            format!("{status}: {}", output.stderr)
        };
        Err(DesktopError::Backend(format!(
            "{PROGRAM} failed with {detail}"
        )))
    }

    async fn close(&self, id: &str) -> Result<()> {
        // notify-send exposes no notification handle, so there is nothing to close.
        if id.is_empty() {
            return Err(DesktopError::Backend("empty notification id".to_string()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug, Clone)]
    enum FakeOutcome {
        Success,
        Exit { code: i32, stderr: String },
        Spawn(io::ErrorKind),
    }

    struct FakeRunner {
        outcome: FakeOutcome,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeRunner {
        fn new(outcome: FakeOutcome) -> Arc<Self> {
            Arc::new(Self {
                outcome,
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
            match &self.outcome {
                FakeOutcome::Success => Ok(CommandOutput {
                    code: Some(0),
                    stderr: String::new(),
                }),
                FakeOutcome::Exit { code, stderr } => Ok(CommandOutput {
                    code: Some(*code),
                    stderr: stderr.clone(),
                }),
                FakeOutcome::Spawn(kind) => Err(io::Error::new(*kind, "no such program")),
            }
        }
    }

    fn notifier(outcome: FakeOutcome) -> (CommandNotifier, Arc<FakeRunner>) {
        let runner = FakeRunner::new(outcome);
        (CommandNotifier::with_runner(Arc::clone(&runner)), runner)
    }

    #[tokio::test]
    async fn notification_passes_app_name_title_and_body() {
        let (notifier, runner) = notifier(FakeOutcome::Success);
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
                    "--".to_string(),
                    "Message".to_string(),
                    "Hello".to_string(),
                ],
            )]
        );
    }

    #[tokio::test]
    async fn urgency_is_mapped() {
        let (notifier, runner) = notifier(FakeOutcome::Success);
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
    async fn close_is_a_no_op_success() {
        let (notifier, runner) = notifier(FakeOutcome::Success);
        notifier.close("42").await.unwrap();
        assert!(notifier.close("").await.is_err(), "empty id is rejected");
        assert!(runner.calls().is_empty(), "close must not spawn a process");
    }

    #[test]
    fn desktop_accepts_the_command_notifier() {
        let desktop = crate::Desktop::default().with_notifier(crate::backends::notifier());
        assert!(desktop.capabilities().notifications);
    }
}
