//! Clipboard access through `wl-clipboard` on Wayland and `xclip` on X11.

use std::io;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use crate::error::{DesktopError, Result};
use crate::traits::Clipboard;

/// Clipboard tool set selected from the session type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardFlavor {
    /// `wl-paste` and `wl-copy` from `wl-clipboard`.
    Wayland,
    /// `xclip` on the X11 clipboard selection.
    X11,
}

impl ClipboardFlavor {
    /// Pick Wayland when `WAYLAND_DISPLAY` is set, X11 otherwise.
    #[must_use]
    pub fn detect() -> Self {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            Self::Wayland
        } else {
            Self::X11
        }
    }

    fn read_command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Wayland => ("wl-paste", &["--no-newline"]),
            Self::X11 => ("xclip", &["-selection", "clipboard", "-o"]),
        }
    }

    fn write_command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Wayland => ("wl-copy", &[]),
            Self::X11 => ("xclip", &["-selection", "clipboard"]),
        }
    }

    fn missing_tool(self) -> DesktopError {
        match self {
            Self::Wayland => DesktopError::Unsupported("wl-clipboard is not installed"),
            Self::X11 => DesktopError::Unsupported("xclip is not installed"),
        }
    }
}

/// Captured result of a finished child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Whether the process exited with status zero.
    pub success: bool,
    /// Exit code, or `None` when the process was killed by a signal.
    pub code: Option<i32>,
    /// Bytes written to stdout.
    pub stdout: Vec<u8>,
    /// Bytes written to stderr.
    pub stderr: Vec<u8>,
}

/// Runs one external command. Tests substitute a fake.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `program` with `args`, writing `stdin` when present.
    ///
    /// # Errors
    /// Fails when the process cannot be started or its standard streams cannot
    /// be driven.
    async fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> io::Result<CommandOutput>;
}

/// [`CommandRunner`] backed by [`tokio::process::Command`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioRunner;

#[async_trait]
impl CommandRunner for TokioRunner {
    async fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> io::Result<CommandOutput> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        if let Some(input) = stdin {
            let mut pipe = child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other(format!("{program} stdin was not piped")))?;
            pipe.write_all(input).await?;
        }
        let output = child.wait_with_output().await?;
        Ok(CommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// Clipboard access through the session's command-line clipboard tools.
pub struct CommandClipboard {
    flavor: ClipboardFlavor,
    runner: Arc<dyn CommandRunner>,
}

impl CommandClipboard {
    /// Build a clipboard adapter for an explicit tool set and runner.
    #[must_use]
    pub fn new(flavor: ClipboardFlavor, runner: Arc<dyn CommandRunner>) -> Self {
        Self { flavor, runner }
    }

    /// Build a clipboard adapter for the current session.
    #[must_use]
    pub fn detect() -> Self {
        Self::new(ClipboardFlavor::detect(), Arc::new(TokioRunner))
    }

    async fn run(
        &self,
        program: &'static str,
        args: &'static [&'static str],
        stdin: Option<&[u8]>,
    ) -> Result<CommandOutput> {
        let output = self
            .runner
            .run(program, args, stdin)
            .await
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    self.flavor.missing_tool()
                } else {
                    DesktopError::Backend(format!("failed to run {program}: {error}"))
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
                format!("{program} exited with {code}")
            } else {
                format!("{program} exited with {code}: {stderr}")
            };
            return Err(DesktopError::Backend(detail));
        }
        Ok(output)
    }
}

#[async_trait]
impl Clipboard for CommandClipboard {
    async fn read_text(&self) -> Result<Option<String>> {
        let (program, args) = self.flavor.read_command();
        let output = self.run(program, args, None).await?;
        if output.stdout.is_empty() {
            return Ok(None);
        }
        String::from_utf8(output.stdout)
            .map(Some)
            .map_err(|_| DesktopError::Backend(format!("{program} returned non-UTF-8 text")))
    }

    async fn write_text(&self, text: &str) -> Result<()> {
        let (program, args) = self.flavor.write_command();
        self.run(program, args, Some(text.as_bytes())).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Call {
        program: String,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    }

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
        calls: Mutex<Vec<Call>>,
    }

    impl FakeRunner {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            program: &str,
            args: &[&str],
            stdin: Option<&[u8]>,
        ) -> io::Result<CommandOutput> {
            self.calls.lock().unwrap().push(Call {
                program: program.to_string(),
                args: args.iter().map(|arg| arg.to_string()).collect(),
                stdin: stdin.map(<[u8]>::to_vec),
            });
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

    fn clipboard(flavor: ClipboardFlavor, reply: Reply) -> (CommandClipboard, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner {
            reply,
            calls: Mutex::new(Vec::new()),
        });
        let clipboard = CommandClipboard::new(flavor, Arc::clone(&runner));
        (clipboard, runner)
    }

    fn call(program: &str, args: &[&str], stdin: Option<&[u8]>) -> Call {
        Call {
            program: program.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            stdin: stdin.map(<[u8]>::to_vec),
        }
    }

    #[tokio::test]
    async fn wayland_read_uses_wl_paste() {
        let (clipboard, runner) = clipboard(
            ClipboardFlavor::Wayland,
            Reply::Output {
                code: 0,
                stdout: b"hello".to_vec(),
                stderr: Vec::new(),
            },
        );

        assert_eq!(
            clipboard.read_text().await.unwrap(),
            Some("hello".to_string())
        );
        assert_eq!(
            runner.calls(),
            vec![call("wl-paste", &["--no-newline"], None)]
        );
    }

    #[tokio::test]
    async fn wayland_write_uses_wl_copy() {
        let (clipboard, runner) = clipboard(
            ClipboardFlavor::Wayland,
            Reply::Output {
                code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        );

        clipboard.write_text("hi").await.unwrap();
        assert_eq!(runner.calls(), vec![call("wl-copy", &[], Some(b"hi"))]);
    }

    #[tokio::test]
    async fn x11_read_uses_xclip_output() {
        let (clipboard, runner) = clipboard(
            ClipboardFlavor::X11,
            Reply::Output {
                code: 0,
                stdout: b"hello".to_vec(),
                stderr: Vec::new(),
            },
        );

        assert_eq!(
            clipboard.read_text().await.unwrap(),
            Some("hello".to_string())
        );
        assert_eq!(
            runner.calls(),
            vec![call("xclip", &["-selection", "clipboard", "-o"], None)]
        );
    }

    #[tokio::test]
    async fn x11_write_uses_xclip_input() {
        let (clipboard, runner) = clipboard(
            ClipboardFlavor::X11,
            Reply::Output {
                code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        );

        clipboard.write_text("hi").await.unwrap();
        assert_eq!(
            runner.calls(),
            vec![call("xclip", &["-selection", "clipboard"], Some(b"hi"))]
        );
    }

    #[tokio::test]
    async fn missing_tool_is_unsupported() {
        let (clipboard, _) = clipboard(ClipboardFlavor::X11, Reply::Missing);

        let error = clipboard.read_text().await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Unsupported(message) if message.contains("xclip")),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_backend_error() {
        let (clipboard, _) = clipboard(
            ClipboardFlavor::Wayland,
            Reply::Output {
                code: 1,
                stdout: Vec::new(),
                stderr: b"No selection".to_vec(),
            },
        );

        let error = clipboard.read_text().await.unwrap_err();
        assert!(
            matches!(
                &error,
                DesktopError::Backend(message)
                    if message.contains("wl-paste") && message.contains("No selection")
            ),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn non_utf8_output_is_backend_error() {
        let (clipboard, _) = clipboard(
            ClipboardFlavor::X11,
            Reply::Output {
                code: 0,
                stdout: vec![0xff, 0xfe],
                stderr: Vec::new(),
            },
        );

        let error = clipboard.read_text().await.unwrap_err();
        assert!(
            matches!(
                &error,
                DesktopError::Backend(message) if message.contains("non-UTF-8")
            ),
            "unexpected error: {error}"
        );
    }
}
