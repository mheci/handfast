//! System output volume through `wpctl`.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::VolumeState;
use crate::traits::{AudioSink, Volume};

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

    async fn sinks(&self) -> Result<Vec<AudioSink>> {
        let status = self.run(&["status"]).await?;
        let mut sinks = parse_sinks(&status.stdout)?;
        if sinks.is_empty() {
            // A host without a parseable status still gets one entry: the
            // default sink, which `get` already understands.
            let state = self.get().await?;
            return Ok(vec![AudioSink {
                name: "default".to_string(),
                description: "System volume".to_string(),
                volume: i64::from(state.percent),
                max_volume: 100,
                muted: state.muted,
                enabled: true,
            }]);
        }
        for sink in &mut sinks {
            // `wpctl` reports fractions, so read each sink back for the exact
            // value instead of trusting the status line's bracketed number.
            let id = sink.name.clone();
            if let Ok(output) = self.run(&["get-volume", id.as_str()]).await {
                if let Ok(state) = parse_volume(&output.stdout) {
                    sink.volume = i64::from(state.percent);
                    sink.muted = state.muted;
                }
            }
        }
        Ok(sinks)
    }

    async fn set_sink(&self, name: &str, volume: Option<i64>, muted: Option<bool>) -> Result<()> {
        if let Some(volume) = volume {
            let percent = format!("{}%", volume.clamp(0, 100));
            self.run(&["set-volume", name, percent.as_str()]).await?;
        }
        if let Some(muted) = muted {
            self.run(&["set-mute", name, if muted { "1" } else { "0" }])
                .await?;
        }
        Ok(())
    }
}

/// Parse one `wpctl status` line describing a sink.
///
/// A status block line looks like
/// `│  *   47. Built-in Audio Analog Stereo   [vol: 0.45]`.
/// A `*` marks the default sink. The id is the numeric token before the `.` and
/// the name is everything after it, with the bracketed volume stripped.
fn parse_status_line(line: &str) -> Option<(bool, String, String)> {
    let trimmed = line.trim_start_matches(|character: char| {
        character.is_whitespace() || matches!(character, '│' | '├' | '─' | '└' | '*' | '+')
    });
    // `wpctl` puts the default marker in the first column after the tree glyph.
    let is_default = line
        .trim_start_matches(|character: char| character.is_whitespace() || character == '│')
        .starts_with('*');
    let (id, rest) = trimmed.split_once('.')?;
    if id.trim().parse::<u32>().is_err() {
        return None;
    }
    let mut rest = rest.trim();
    // `wpctl` appends the current volume in brackets.
    if let Some(index) = rest.find("[vol:") {
        rest = rest[..index].trim_end();
    }
    if rest.is_empty() {
        return None;
    }
    Some((is_default, id.trim().to_string(), rest.to_string()))
}

/// Parse `wpctl status` output into the sinks under the `Sinks:` block.
fn parse_sinks(stdout: &[u8]) -> Result<Vec<AudioSink>> {
    let text = std::str::from_utf8(stdout)
        .map_err(|_| DesktopError::Backend("wpctl returned non-UTF-8 output".to_string()))?;
    let mut sinks = Vec::new();
    let mut in_sinks = false;
    for line in text.lines() {
        if line.contains("Sinks:") {
            in_sinks = true;
            continue;
        }
        if line.contains("Sources:")
            || line.contains("Filters:")
            || line.contains("Clients:")
            || line.contains("Streams:")
        {
            // The list of sinks is over; a later `Sinks:` under a stream
            // section must not be mistaken for another device list.
            in_sinks = false;
            continue;
        }
        if !in_sinks {
            continue;
        }
        let Some((enabled, id, description)) = parse_status_line(line) else {
            continue;
        };
        sinks.push(AudioSink {
            name: id,
            description,
            volume: 0,
            max_volume: 100,
            muted: false,
            enabled,
        });
    }
    Ok(sinks)
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

    /// A trimmed capture of real `wpctl status` layout, box characters included.
    const STATUS_FIXTURE: &str =
        "PipeWire 'pipewire-0'\n\nAudio\n\u{251c}\u{2500} Devices:\n\u{2502}  \u{251c}\u{2500} Sinks:\n\u{2502}  \u{2502}  *   47. Built-in Audio Analog Stereo        [vol: 0.45]\n\u{2502}  \u{2502}      52. HDMI Output                        [vol: 1.00]\n\u{2502}  \u{2514}\u{2500} Sources:\n\u{2502}      58. Built-in Audio Analog Stereo        [vol: 1.00]\n";

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

    struct ScriptedRunner {
        replies: Mutex<Vec<Vec<u8>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedRunner {
        fn new(replies: Vec<&[u8]>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into_iter().map(<[u8]>::to_vec).collect()),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for ScriptedRunner {
        async fn run(&self, args: &[&str]) -> io::Result<CommandOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| arg.to_string()).collect());
            let stdout = {
                let mut replies = self.replies.lock().unwrap();
                if replies.is_empty() {
                    Vec::new()
                } else {
                    replies.remove(0)
                }
            };
            Ok(CommandOutput {
                success: true,
                code: Some(0),
                stdout,
                stderr: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn sinks_parse_the_status_block_and_read_each_sink() {
        let runner = ScriptedRunner::new(vec![
            STATUS_FIXTURE.as_bytes(),
            b"Volume: 0.45\n",
            b"Volume: 1.00 [MUTED]\n",
        ]);
        let volume = WpctlVolume::with_runner(Arc::clone(&runner) as Arc<dyn CommandRunner>);

        let sinks = volume.sinks().await.unwrap();

        assert_eq!(sinks.len(), 2, "two sinks, not the source");
        assert_eq!(sinks[0].name, "47");
        assert_eq!(sinks[0].description, "Built-in Audio Analog Stereo");
        assert!(sinks[0].enabled, "the starred sink is the default");
        assert_eq!(sinks[0].volume, 45);
        assert!(!sinks[0].muted);
        assert_eq!(sinks[1].name, "52");
        assert_eq!(sinks[1].description, "HDMI Output");
        assert!(!sinks[1].enabled);
        assert_eq!(sinks[1].volume, 100);
        assert!(sinks[1].muted);
        assert_eq!(
            runner.calls(),
            vec![
                call(&["status"]),
                call(&["get-volume", "47"]),
                call(&["get-volume", "52"]),
            ]
        );
    }

    #[tokio::test]
    async fn set_sink_addresses_the_named_sink() {
        let (volume, runner) = volume(Reply::Output {
            code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });

        volume.set_sink("52", Some(30), Some(true)).await.unwrap();

        assert_eq!(
            runner.calls(),
            vec![
                call(&["set-volume", "52", "30%"]),
                call(&["set-mute", "52", "1"]),
            ]
        );
    }

    #[tokio::test]
    async fn set_sink_without_a_volume_only_toggles_mute() {
        let (volume, runner) = volume(Reply::Output {
            code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });

        volume.set_sink("47", None, Some(false)).await.unwrap();

        assert_eq!(runner.calls(), vec![call(&["set-mute", "47", "0"])]);
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
