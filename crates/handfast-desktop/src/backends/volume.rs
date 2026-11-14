//! System output volume through `wpctl`.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::VolumeState;
use crate::traits::Volume;

const PROGRAM: &str = "wpctl";
const SINK: &str = "@DEFAULT_AUDIO_SINK@";

/// Captured result of a finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Whether the process exited with status zero.
    pub success: bool,
    /// Exit code, or `None` when the process ended without one.
    pub code: Option<i32>,
    /// Bytes written to stdout.
    pub stdout: Vec<u8>,
    /// Bytes written to stderr.
    pub stderr: Vec<u8>,
}

/// Runs one `wpctl` command. Tests substitute a fake.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `wpctl` with `args`.
    ///
    /// # Errors
    /// Fails when the process cannot be started.
    async fn run(&self, args: &[&str]) -> io::Result<CommandOutput>;
}

/// [`CommandRunner`] backed by [`tokio::process::Command`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioRunner;

#[async_trait]
impl CommandRunner for TokioRunner {
    async fn run(&self, args: &[&str]) -> io::Result<CommandOutput> {
        let output = tokio::process::Command::new(PROGRAM)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(CommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// System output volume through `wpctl`.
pub struct WpctlVolume {
    runner: Arc<dyn CommandRunner>,
}

impl WpctlVolume {
    /// Build a volume adapter that runs the real `wpctl`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioRunner),
        }
    }

    /// Build a volume adapter over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    async fn run(&self, args: &[&str]) -> Result<CommandOutput> {
        let output = self.runner.run(args).await.map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                DesktopError::Unsupported("wpctl is not installed")
            } else {
                DesktopError::Backend(format!("failed to run {PROGRAM}: {error}"))
            }
        })?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let code = match output.code {
                Some(code) => code.to_string(),
                None => "a signal".to_string(),
            };
            let detail = if stderr.is_empty() {
                format!("{PROGRAM} exited with {code}")
            } else {
                format!("{PROGRAM} exited with {code}: {stderr}")
            };
            return Err(DesktopError::Backend(detail));
        }
        Ok(output)
    }
}

impl Default for WpctlVolume {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Volume for WpctlVolume {
    async fn get(&self) -> Result<VolumeState> {
        let output = self.run(&["get-volume", SINK]).await?;
        parse_volume(&output.stdout)
    }

    async fn set(&self, percent: u8, muted: bool) -> Result<()> {
        let percent = format!("{percent}%");
        self.run(&["set-volume", SINK, percent.as_str()]).await?;
        self.run(&["set-mute", SINK, if muted { "1" } else { "0" }])
            .await?;
        Ok(())
    }
}

fn unexpected(text: &str) -> DesktopError {
    DesktopError::Backend(format!("wpctl returned an unexpected volume line: {text}"))
}

/// Parse `Volume: 0.50 [MUTED]` into a percentage and mute flag.
fn parse_volume(stdout: &[u8]) -> Result<VolumeState> {
    let text = std::str::from_utf8(stdout)
        .map_err(|_| DesktopError::Backend("wpctl returned non-UTF-8 output".to_string()))?;
    let text = text.trim();
    let rest = text
        .strip_prefix("Volume:")
        .ok_or_else(|| unexpected(text))?;
    let muted = rest.contains("[MUTED]");
    let value = rest
        .split_whitespace()
        .next()
        .ok_or_else(|| unexpected(text))?;
    let value: f64 = value.parse().map_err(|_| unexpected(text))?;
    let percent = (value * 100.0).round().clamp(0.0, 100.0) as u8;
    Ok(VolumeState { percent, muted })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    enum Reply {
        Output {
            code: i32,
            stdout: Vec<u8>,
            stderr: Vec<u8>,
        },
        Missing,
    }

    struct FakeRunner {
        reply: Reply,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(&self, args: &[&str]) -> io::Result<CommandOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| arg.to_string()).collect());
            match &self.reply {
                Reply::Missing => Err(io::Error::from(io::ErrorKind::NotFound)),
                Reply::Output {
                    code,
                    stdout,
                    stderr,
                } => Ok(CommandOutput {
                    success: *code == 0,
                    code: Some(*code),
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                }),
            }
        }
    }

    fn volume(reply: Reply) -> (WpctlVolume, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner {
            reply,
            calls: Mutex::new(Vec::new()),
        });
        let volume = WpctlVolume::with_runner(Arc::clone(&runner) as Arc<dyn CommandRunner>);
        (volume, runner)
    }

    fn call(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[tokio::test]
    async fn get_parses_percent_and_mute() {
        let (volume, runner) = volume(Reply::Output {
            code: 0,
            stdout: b"Volume: 0.50 [MUTED]\n".to_vec(),
            stderr: Vec::new(),
        });

        assert_eq!(
            volume.get().await.unwrap(),
            VolumeState {
                percent: 50,
                muted: true
            }
        );
        assert_eq!(
            runner.calls(),
            vec![call(&["get-volume", "@DEFAULT_AUDIO_SINK@"])]
        );
    }

    #[tokio::test]
    async fn get_reads_an_unmuted_sink() {
        let (volume, _) = volume(Reply::Output {
            code: 0,
            stdout: b"Volume: 0.75\n".to_vec(),
            stderr: Vec::new(),
        });

        assert_eq!(
            volume.get().await.unwrap(),
            VolumeState {
                percent: 75,
                muted: false
            }
        );
    }

    #[tokio::test]
    async fn set_sends_volume_then_mute() {
        let (volume, runner) = volume(Reply::Output {
            code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });

        volume.set(40, true).await.unwrap();
        assert_eq!(
            runner.calls(),
            vec![
                call(&["set-volume", "@DEFAULT_AUDIO_SINK@", "40%"]),
                call(&["set-mute", "@DEFAULT_AUDIO_SINK@", "1"]),
            ]
        );
    }

    #[tokio::test]
    async fn set_unmuted_sends_zero() {
        let (volume, runner) = volume(Reply::Output {
            code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });

        volume.set(40, false).await.unwrap();
        assert_eq!(
            runner.calls(),
            vec![
                call(&["set-volume", "@DEFAULT_AUDIO_SINK@", "40%"]),
                call(&["set-mute", "@DEFAULT_AUDIO_SINK@", "0"]),
            ]
        );
    }

    #[tokio::test]
    async fn missing_tool_is_unsupported() {
        let (volume, _) = volume(Reply::Missing);

        let error = volume.get().await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Unsupported(message) if message.contains("wpctl")),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_backend_error() {
        let (volume, _) = volume(Reply::Output {
            code: 1,
            stdout: Vec::new(),
            stderr: b"No such sink".to_vec(),
        });

        let error = volume.get().await.unwrap_err();
        assert!(
            matches!(
                &error,
                DesktopError::Backend(message)
                    if message.contains("wpctl") && message.contains("No such sink")
            ),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn malformed_output_is_backend_error() {
        let (volume, _) = volume(Reply::Output {
            code: 0,
            stdout: b"not a volume line".to_vec(),
            stderr: Vec::new(),
        });

        let error = volume.get().await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Backend(message) if message.contains("unexpected")),
            "unexpected error: {error}"
        );
    }
}
