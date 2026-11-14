//! MPRIS media control through `playerctl`.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::{MediaAction, PlaybackState};
use crate::traits::Media;

use super::clipboard::{CommandOutput, CommandRunner, TokioRunner};

const PROGRAM: &str = "playerctl";

// Real tabs: playerctl prints text outside {{ }} verbatim, so `\t` would be
// literal backslash-t.
const METADATA_FORMAT: &str = "{{title}}\t{{artist}}\t{{position}}\t{{mpris:length}}";

/// MPRIS media control through the `playerctl` command line tool.
pub struct PlayerctlMedia {
    runner: Arc<dyn CommandRunner>,
}

impl PlayerctlMedia {
    /// Build a backend that runs the real `playerctl` binary.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioRunner),
        }
    }

    /// Build a backend over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    async fn call(&self, args: &[&str]) -> Result<CommandOutput> {
        self.runner.run(PROGRAM, args, None).await.map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                DesktopError::Unsupported("playerctl is not installed")
            } else {
                DesktopError::Backend(format!("failed to run {PROGRAM}: {error}"))
            }
        })
    }
}

impl Default for PlayerctlMedia {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Media for PlayerctlMedia {
    async fn players(&self) -> Result<Vec<String>> {
        let output = checked(&["-l"], self.call(&["-l"]).await?)?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect())
    }

    async fn playback(&self, player: &str) -> Result<Option<PlaybackState>> {
        let status_args = ["-p", player, "status"];
        let output = checked(&status_args, self.call(&status_args).await?)?;
        let status = first_line(&output.stdout);
        if status.is_empty() {
            return Ok(None);
        }

        let metadata_args = ["-p", player, "metadata", "--format", METADATA_FORMAT];
        let metadata = self.call(&metadata_args).await?;
        // playerctl exits non-zero when the player has no current track. That is
        // an empty snapshot, not a backend failure.
        let line = if metadata.success {
            first_line(&metadata.stdout)
        } else {
            String::new()
        };
        let mut fields = line.split('\t');
        Ok(Some(PlaybackState {
            player: player.to_string(),
            playing: status == "Playing",
            title: field(fields.next()),
            artist: field(fields.next()),
            position_ms: millis(fields.next()),
            length_ms: millis(fields.next()),
            volume: None,
        }))
    }

    async fn control(&self, player: &str, action: MediaAction) -> Result<()> {
        let command = match action {
            MediaAction::Play => "play",
            MediaAction::Pause => "pause",
            MediaAction::PlayPause => "play-pause",
            MediaAction::Stop => "stop",
            MediaAction::Next => "next",
            MediaAction::Previous => "previous",
        };
        let args = ["-p", player, command];
        checked(&args, self.call(&args).await?)?;
        Ok(())
    }

    async fn set_volume(&self, player: &str, percent: u8) -> Result<()> {
        let level = format!("{:.2}", f64::from(percent.min(100)) / 100.0);
        let args = ["-p", player, "volume", level.as_str()];
        checked(&args, self.call(&args).await?)?;
        Ok(())
    }
}

fn checked(args: &[&str], output: CommandOutput) -> Result<CommandOutput> {
    if output.success {
        Ok(output)
    } else {
        Err(failure(args, &output))
    }
}

fn failure(args: &[&str], output: &CommandOutput) -> DesktopError {
    let code = output
        .code
        .map_or_else(|| "a signal".to_string(), |code| code.to_string());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let detail = if stderr.is_empty() {
        format!("{PROGRAM} {} exited with {code}", args.join(" "))
    } else {
        format!("{PROGRAM} {} exited with {code}: {stderr}", args.join(" "))
    };
    DesktopError::Backend(detail)
}

fn first_line(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

fn field(value: Option<&str>) -> Option<String> {
    value.filter(|value| !value.is_empty()).map(str::to_string)
}

fn millis(value: Option<&str>) -> i64 {
    value
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0)
        / 1000
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Call {
        program: String,
        args: Vec<String>,
    }

    enum Reply {
        Output {
            code: i32,
            stdout: String,
            stderr: String,
        },
        Missing,
    }

    struct FakeRunner {
        replies: Mutex<VecDeque<Reply>>,
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
            _stdin: Option<&[u8]>,
        ) -> io::Result<CommandOutput> {
            self.calls.lock().unwrap().push(Call {
                program: program.to_string(),
                args: args.iter().map(|arg| arg.to_string()).collect(),
            });
            match self.replies.lock().unwrap().pop_front() {
                None => Ok(output(0, "", "")),
                Some(Reply::Missing) => Err(io::Error::from(io::ErrorKind::NotFound)),
                Some(Reply::Output {
                    code,
                    stdout,
                    stderr,
                }) => Ok(output(code, &stdout, &stderr)),
            }
        }
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            success: code == 0,
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn success(stdout: &str) -> Reply {
        Reply::Output {
            code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn failure_reply(code: i32, stderr: &str) -> Reply {
        Reply::Output {
            code,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    fn backend(replies: Vec<Reply>) -> (PlayerctlMedia, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner {
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(Vec::new()),
        });
        let media = PlayerctlMedia::with_runner(Arc::clone(&runner) as Arc<dyn CommandRunner>);
        (media, runner)
    }

    fn call(args: &[&str]) -> Call {
        Call {
            program: "playerctl".to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn players_lists_running_players() {
        let (media, runner) = backend(vec![success("vlc\nspotify\n\n")]);

        assert_eq!(
            media.players().await.unwrap(),
            vec!["vlc".to_string(), "spotify".to_string()]
        );
        assert_eq!(runner.calls(), vec![call(&["-l"])]);
    }

    #[tokio::test]
    async fn playback_parses_status_and_metadata() {
        let (media, runner) = backend(vec![
            success("Playing\n"),
            success("Song\tBand\t1234000\t245000\n"),
        ]);

        let state = media.playback("vlc").await.unwrap().unwrap();
        assert_eq!(
            state,
            PlaybackState {
                player: "vlc".to_string(),
                playing: true,
                title: Some("Song".to_string()),
                artist: Some("Band".to_string()),
                position_ms: 1234,
                length_ms: 245,
                volume: None,
            }
        );
        assert_eq!(
            runner.calls(),
            vec![
                call(&["-p", "vlc", "status"]),
                call(&["-p", "vlc", "metadata", "--format", METADATA_FORMAT]),
            ]
        );
    }

    #[tokio::test]
    async fn playback_missing_metadata_is_none() {
        let (media, _) = backend(vec![success("Paused\n"), success("\t\t\t\n")]);

        let state = media.playback("vlc").await.unwrap().unwrap();
        assert!(!state.playing);
        assert_eq!(state.title, None);
        assert_eq!(state.artist, None);
        assert_eq!(state.position_ms, 0);
        assert_eq!(state.length_ms, 0);
    }

    #[tokio::test]
    async fn playback_without_a_track_is_not_an_error() {
        let (media, _) = backend(vec![
            success("Stopped\n"),
            failure_reply(1, "No player could handle this command\n"),
        ]);

        let state = media.playback("vlc").await.unwrap().unwrap();
        assert!(!state.playing);
        assert_eq!(state.title, None);
        assert_eq!(state.artist, None);
    }

    #[tokio::test]
    async fn playback_missing_player_is_backend_error() {
        let (media, _) = backend(vec![failure_reply(1, "No players found\n")]);

        let error = media.playback("vlc").await.unwrap_err();
        assert!(
            matches!(
                &error,
                DesktopError::Backend(message)
                    if message.contains("vlc") && message.contains("No players found")
            ),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn control_maps_every_action() {
        let (media, runner) = backend(Vec::new());

        for (action, command) in [
            (MediaAction::Play, "play"),
            (MediaAction::Pause, "pause"),
            (MediaAction::PlayPause, "play-pause"),
            (MediaAction::Stop, "stop"),
            (MediaAction::Next, "next"),
            (MediaAction::Previous, "previous"),
        ] {
            media.control("vlc", action).await.unwrap();
            assert_eq!(
                runner.calls().last().unwrap(),
                &call(&["-p", "vlc", command])
            );
        }
        assert_eq!(runner.calls().len(), 6);
    }

    #[tokio::test]
    async fn volume_clamps_and_maps_to_a_fraction() {
        let (media, runner) = backend(Vec::new());

        for (percent, level) in [(0, "0.00"), (50, "0.50"), (100, "1.00"), (255, "1.00")] {
            media.set_volume("vlc", percent).await.unwrap();
            assert_eq!(
                runner.calls().last().unwrap(),
                &call(&["-p", "vlc", "volume", level])
            );
        }
    }

    #[tokio::test]
    async fn nonzero_exit_is_backend_error() {
        let (media, _) = backend(vec![failure_reply(3, "boom\n")]);

        let error = media.control("vlc", MediaAction::Play).await.unwrap_err();
        match error {
            DesktopError::Backend(message) => {
                assert!(message.contains("playerctl"), "{message}");
                assert!(message.contains("exited with 3"), "{message}");
                assert!(message.contains("boom"), "{message}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_tool_is_unsupported() {
        let (media, _) = backend(vec![Reply::Missing]);

        let error = media.players().await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Unsupported(message) if message.contains("playerctl")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn desktop_accepts_the_playerctl_backend() {
        let desktop = crate::Desktop::default().with_media(crate::backends::media());
        assert!(desktop.capabilities().media);
    }
}
