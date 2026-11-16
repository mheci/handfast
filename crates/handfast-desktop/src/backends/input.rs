//! Pointer and keyboard injection through `ydotool` and `wtype`.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;

use crate::backends::clipboard::{CommandOutput, CommandRunner, TokioRunner};
use crate::error::{DesktopError, Result};
use crate::model::{ButtonAction, KeyEvent, Modifiers, PointerButton, PointerEvent};
use crate::traits::Input;

const YDOTOOL: &str = "ydotool";
const WTYPE: &str = "wtype";

fn failure(program: &str, output: &CommandOutput) -> DesktopError {
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
    DesktopError::Backend(detail)
}

fn click_codes(button: PointerButton) -> (&'static str, &'static str, &'static str) {
    match button {
        PointerButton::Left => ("0x40", "0x80", "0xC0"),
        PointerButton::Right => ("0x41", "0x81", "0xC1"),
        PointerButton::Middle => ("0x42", "0x82", "0xC2"),
    }
}

fn special_key(code: u8) -> Option<&'static str> {
    Some(match code {
        1 => "BackSpace",
        2 => "Tab",
        3 => "Linefeed",
        4 => "Left",
        5 => "Up",
        6 => "Right",
        7 => "Down",
        8 => "Prior",
        9 => "Next",
        10 => "Home",
        11 => "End",
        12 => "Return",
        13 => "Delete",
        14 => "Escape",
        15 => "Sys_Req",
        16 => "Scroll_Lock",
        17 => "Control_L",
        18 => "Alt_L",
        19 => "Shift_L",
        20 => "Super_L",
        21 => "F1",
        22 => "F2",
        23 => "F3",
        24 => "F4",
        25 => "F5",
        26 => "F6",
        27 => "F7",
        28 => "F8",
        29 => "F9",
        30 => "F10",
        31 => "F11",
        32 => "F12",
        _ => return None,
    })
}

fn modifier_names(modifiers: Modifiers) -> Vec<&'static str> {
    let mut names = Vec::new();
    if modifiers.ctrl {
        names.push("ctrl");
    }
    if modifiers.alt {
        names.push("alt");
    }
    if modifiers.shift {
        names.push("shift");
    }
    if modifiers.sup {
        names.push("super");
    }
    names
}

/// Injects input through `ydotool` for pointers and `wtype` for keys.
pub struct WaylandInput {
    runner: Arc<dyn CommandRunner>,
}

impl WaylandInput {
    /// Build an input adapter that runs the session tools.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioRunner),
        }
    }

    /// Build an input adapter over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    async fn run(&self, program: &str, args: &[String]) -> Result<()> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self
            .runner
            .run(program, &args, None)
            .await
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    DesktopError::Unsupported("the input tools are not installed")
                } else {
                    DesktopError::Backend(format!("failed to run {program}: {error}"))
                }
            })?;
        if !output.success {
            return Err(failure(program, &output));
        }
        Ok(())
    }
}

impl Default for WaylandInput {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Input for WaylandInput {
    async fn pointer(&self, event: PointerEvent) -> Result<()> {
        let mut args = vec!["mousemove".to_string()];
        if event.scroll {
            args.push("-w".to_string());
        }
        args.push("-x".to_string());
        args.push(event.dx.round().to_string());
        args.push("-y".to_string());
        args.push(event.dy.round().to_string());
        self.run(YDOTOOL, &args).await
    }

    async fn button(&self, button: PointerButton, action: ButtonAction) -> Result<()> {
        let (press, release, click) = click_codes(button);
        let mut args = vec!["click".to_string()];
        match action {
            ButtonAction::Click => args.push(click.to_string()),
            ButtonAction::DoubleClick => {
                args.push("--repeat".to_string());
                args.push("2".to_string());
                args.push(click.to_string());
            }
            ButtonAction::Press => args.push(press.to_string()),
            ButtonAction::Release => args.push(release.to_string()),
        }
        self.run(YDOTOOL, &args).await
    }

    async fn key(&self, event: KeyEvent) -> Result<()> {
        match event {
            KeyEvent::Text { text, modifiers } => {
                let names = modifier_names(modifiers);
                let mut args = Vec::new();
                if names.is_empty() {
                    args.push("--".to_string());
                    args.push(text);
                } else {
                    if text.starts_with('-') {
                        return Err(DesktopError::Backend(
                            "refusing to type text that looks like an option".to_string(),
                        ));
                    }
                    for name in &names {
                        args.push("-M".to_string());
                        args.push((*name).to_string());
                    }
                    args.push(text);
                    for name in names.iter().rev() {
                        args.push("-m".to_string());
                        args.push((*name).to_string());
                    }
                }
                self.run(WTYPE, &args).await
            }
            KeyEvent::Special { code, modifiers } => {
                let keysym = special_key(code).ok_or(DesktopError::Unsupported(
                    "the special key is not supported",
                ))?;
                let names = modifier_names(modifiers);
                let mut args = Vec::new();
                for name in &names {
                    args.push("-M".to_string());
                    args.push((*name).to_string());
                }
                args.push("-P".to_string());
                args.push(keysym.to_string());
                args.push("-p".to_string());
                args.push(keysym.to_string());
                for name in names.iter().rev() {
                    args.push("-m".to_string());
                    args.push((*name).to_string());
                }
                self.run(WTYPE, &args).await
            }
        }
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
    }

    struct FakeRunner {
        output: Mutex<Option<io::Result<CommandOutput>>>,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeRunner {
        fn returning(output: io::Result<CommandOutput>) -> Arc<Self> {
            Arc::new(Self {
                output: Mutex::new(Some(output)),
                calls: Mutex::new(Vec::new()),
            })
        }

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
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
            });
            self.output
                .lock()
                .unwrap()
                .take()
                .expect("the fake runner is called once")
        }
    }

    fn ok() -> io::Result<CommandOutput> {
        Ok(CommandOutput {
            success: true,
            code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }

    fn adapter(output: io::Result<CommandOutput>) -> (WaylandInput, Arc<FakeRunner>) {
        let runner = FakeRunner::returning(output);
        let input = WaylandInput::with_runner(Arc::clone(&runner) as Arc<dyn CommandRunner>);
        (input, runner)
    }

    #[tokio::test]
    async fn pointer_moves_with_ydotool() {
        let (input, runner) = adapter(ok());

        input
            .pointer(PointerEvent {
                dx: 3.4,
                dy: -2.0,
                scroll: false,
            })
            .await
            .unwrap();

        assert_eq!(
            runner.calls(),
            vec![Call {
                program: "ydotool".to_string(),
                args: ["mousemove", "-x", "3", "-y", "-2"]
                    .iter()
                    .map(|arg| (*arg).to_string())
                    .collect(),
            }]
        );
    }

    #[tokio::test]
    async fn scroll_uses_the_wheel() {
        let (input, runner) = adapter(ok());

        input
            .pointer(PointerEvent {
                dx: 0.0,
                dy: -1.0,
                scroll: true,
            })
            .await
            .unwrap();

        assert_eq!(
            runner.calls()[0].args,
            ["mousemove", "-w", "-x", "0", "-y", "-1"]
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn clicks_use_the_button_code() {
        let (input, runner) = adapter(ok());
        input
            .button(PointerButton::Right, ButtonAction::Click)
            .await
            .unwrap();
        assert_eq!(
            runner.calls()[0].args,
            ["click", "0xC1"]
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn double_click_repeats() {
        let (input, runner) = adapter(ok());
        input
            .button(PointerButton::Left, ButtonAction::DoubleClick)
            .await
            .unwrap();
        assert_eq!(
            runner.calls()[0].args,
            ["click", "--repeat", "2", "0xC0"]
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn press_and_release_use_the_masks() {
        let (input, runner) = adapter(ok());
        input
            .button(PointerButton::Middle, ButtonAction::Press)
            .await
            .unwrap();
        assert_eq!(runner.calls()[0].args[1], "0x42");

        let (input, runner) = adapter(ok());
        input
            .button(PointerButton::Middle, ButtonAction::Release)
            .await
            .unwrap();
        assert_eq!(runner.calls()[0].args[1], "0x82");
    }

    #[tokio::test]
    async fn text_uses_wtype() {
        let (input, runner) = adapter(ok());
        input
            .key(KeyEvent::Text {
                text: "hello".to_string(),
                modifiers: Modifiers::default(),
            })
            .await
            .unwrap();
        assert_eq!(
            runner.calls()[0],
            Call {
                program: "wtype".to_string(),
                args: ["--", "hello"]
                    .iter()
                    .map(|arg| (*arg).to_string())
                    .collect(),
            }
        );
    }

    #[tokio::test]
    async fn modified_text_holds_and_releases_modifiers() {
        let (input, runner) = adapter(ok());
        input
            .key(KeyEvent::Text {
                text: "c".to_string(),
                modifiers: Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Modifiers::default()
                },
            })
            .await
            .unwrap();
        assert_eq!(
            runner.calls()[0].args,
            ["-M", "ctrl", "-M", "shift", "c", "-m", "shift", "-m", "ctrl"]
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn special_key_maps_to_a_keysym() {
        let (input, runner) = adapter(ok());
        input
            .key(KeyEvent::Special {
                code: 4,
                modifiers: Modifiers::default(),
            })
            .await
            .unwrap();
        assert_eq!(
            runner.calls()[0].args,
            ["-P", "Left", "-p", "Left"]
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn unknown_special_key_is_unsupported() {
        let (input, _) = adapter(ok());
        let error = input
            .key(KeyEvent::Special {
                code: 33,
                modifiers: Modifiers::default(),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, DesktopError::Unsupported(_)));
    }

    #[tokio::test]
    async fn option_like_modified_text_is_refused() {
        let (input, _) = adapter(ok());
        let error = input
            .key(KeyEvent::Text {
                text: "-x".to_string(),
                modifiers: Modifiers {
                    ctrl: true,
                    ..Modifiers::default()
                },
            })
            .await
            .unwrap_err();
        assert!(matches!(error, DesktopError::Backend(_)));
    }

    #[tokio::test]
    async fn missing_tool_is_unsupported() {
        let (input, _) = adapter(Err(io::Error::from(io::ErrorKind::NotFound)));
        let error = input
            .pointer(PointerEvent {
                dx: 1.0,
                dy: 1.0,
                scroll: false,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, DesktopError::Unsupported(_)));
    }

    #[tokio::test]
    async fn failed_command_is_a_backend_error() {
        let (input, _) = adapter(Ok(CommandOutput {
            success: false,
            code: Some(1),
            stdout: Vec::new(),
            stderr: b"no ydotoold".to_vec(),
        }));
        let error = input
            .pointer(PointerEvent {
                dx: 1.0,
                dy: 1.0,
                scroll: false,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&error, DesktopError::Backend(message) if message.contains("no ydotoold")),
            "unexpected error: {error}"
        );
    }
}
