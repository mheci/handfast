//! Synthetic input through `xdotool`.
//!
//! The desktop client injects pointer and keyboard events itself: XTest and
//! libfakekey on X11, a Wayland protocol on Wayland. Handfast keeps the daemon
//! free of those dependencies by driving `xdotool`, which is the same XTest
//! machinery behind a stable command line. Every action in [`PointerEvent`] and
//! [`KeyEvent`] maps to one invocation, so a remote drag, click or keystroke
//! behaves the same as the desktop client's.
//!
//! `xdotool` needs an X display. On a Wayland session without XWayland the
//! adapter is simply not installed, and the mousepad plugin stays disabled
//! rather than advertising input it cannot inject.

use std::io;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::{KeyEvent, PointerAction, PointerButton, PointerEvent};
use crate::traits::Input;

/// The program used for every injection.
pub const PROGRAM: &str = "xdotool";

/// Starts one external process. Tests substitute a fake.
pub trait Runner: Send + Sync {
    /// Run `program` with `args` to completion.
    ///
    /// # Errors
    /// Fails when the process cannot be started or exited non-zero.
    fn run(&self, program: &str, args: &[String]) -> io::Result<()>;
}

/// [`Runner`] backed by [`std::process::Command`] on a blocking pool.
///
/// `xdotool` is a short-lived process, so waiting for it is both cheap and
/// necessary: the next event must not overtake this one.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdRunner;

impl Runner for StdRunner {
    fn run(&self, program: &str, args: &[String]) -> io::Result<()> {
        let status = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("{program} exited with {status}")))
        }
    }
}

/// Keyboard modifier keys, in the order the desktop client presses them.
fn modifier_args(modifiers: crate::model::Modifiers) -> Vec<String> {
    let mut args = Vec::new();
    if modifiers.ctrl {
        args.push("ctrl".to_string());
    }
    if modifiers.alt {
        args.push("alt".to_string());
    }
    if modifiers.shift {
        args.push("shift".to_string());
    }
    if modifiers.sup {
        args.push("super".to_string());
    }
    args
}

/// The `xdotool` command line that types `text` with `modifiers` held.
///
/// The desktop client presses the modifiers, injects every character and
/// releases them again. `xdotool` chains the same steps in one invocation, so
/// a failure cannot leave a modifier stuck down. `type` is used rather than
/// `key` because it handles arbitrary text, and `--` keeps text that starts
/// with a dash from reading as an option.
pub fn text_command(text: &str, modifiers: crate::model::Modifiers) -> Vec<String> {
    let held = modifier_args(modifiers);
    let mut args = Vec::new();
    for modifier in &held {
        args.push("keydown".to_string());
        args.push(modifier.clone());
    }
    args.push("type".to_string());
    if text.starts_with('-') {
        args.push("--".to_string());
    }
    args.push(text.to_string());
    for modifier in held.iter().rev() {
        args.push("keyup".to_string());
        args.push(modifier.clone());
    }
    args
}

/// The `xdotool key` name for a KDE Connect special-key code.
///
/// The table is the desktop client's `SpecialKeysMap`: index 0 is unused, 1-32
/// are backspace through F12. Returning `None` for an unknown code keeps a
/// malformed packet from typing something arbitrary.
#[must_use]
pub fn special_key_name(code: u8) -> Option<&'static str> {
    const NAMES: [&str; 33] = [
        "",
        "BackSpace",
        "Tab",
        "Linefeed",
        "Left",
        "Up",
        "Right",
        "Down",
        "Prior",
        "Next",
        "Home",
        "End",
        "Return",
        "Delete",
        "Escape",
        "Sys_Req",
        "Scroll_Lock",
        "Control_L",
        "Alt_L",
        "Shift_L",
        "Super_L",
        "F1",
        "F2",
        "F3",
        "F4",
        "F5",
        "F6",
        "F7",
        "F8",
        "F9",
        "F10",
        "F11",
        "F12",
    ];
    NAMES
        .get(usize::from(code))
        .copied()
        .filter(|name| !name.is_empty())
}

fn button_number(button: PointerButton) -> &'static str {
    match button {
        PointerButton::Left => "1",
        PointerButton::Middle => "2",
        PointerButton::Right => "3",
    }
}

/// The command line for one pointer event.
///
/// Scroll keeps the desktop client's sign convention: a negative `dy` scrolls
/// down, and `xdotool` button 5 is down.
pub fn pointer_command(event: PointerEvent) -> Vec<String> {
    match event.action {
        PointerAction::Move => vec![
            "mousemove_relative".to_string(),
            "--".to_string(),
            format!("{}", event.dx.round() as i64),
            format!("{}", event.dy.round() as i64),
        ],
        PointerAction::Scroll => {
            // One wheel notch per unit of scroll, as both clients send them.
            let notches = event.dy.abs().round().max(1.0) as u32;
            let button = if event.dy < 0.0 { "5" } else { "4" };
            vec![
                "click".to_string(),
                "--repeat".to_string(),
                notches.to_string(),
                button.to_string(),
            ]
        }
        PointerAction::Click { button, count } => vec![
            "click".to_string(),
            "--repeat".to_string(),
            count.clamp(1, 2).to_string(),
            button_number(button).to_string(),
        ],
        PointerAction::Press(button) => {
            vec!["mousedown".to_string(), button_number(button).to_string()]
        }
        PointerAction::Release(button) => {
            vec!["mouseup".to_string(), button_number(button).to_string()]
        }
    }
}

/// Injection through the `xdotool` command line.
pub struct XdotoolInput {
    runner: Arc<dyn Runner>,
}

impl XdotoolInput {
    /// Build an input adapter that runs the real `xdotool`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(StdRunner),
        }
    }

    /// Build an input adapter over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn Runner>) -> Self {
        Self { runner }
    }

    fn run(&self, args: Vec<String>) -> Result<()> {
        self.runner.run(PROGRAM, &args).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                DesktopError::Unsupported("xdotool is not installed")
            } else {
                DesktopError::Backend(format!("failed to run {PROGRAM}: {error}"))
            }
        })
    }
}

impl Default for XdotoolInput {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Input for XdotoolInput {
    async fn pointer(&self, event: PointerEvent) -> Result<()> {
        self.run(pointer_command(event))
    }

    async fn key(&self, event: KeyEvent) -> Result<()> {
        match event {
            KeyEvent::Text { text, modifiers } => {
                if text.is_empty() {
                    return Err(DesktopError::Backend(
                        "refusing to type an empty string".to_string(),
                    ));
                }
                self.run(text_command(&text, modifiers))
            }
            KeyEvent::Special { code, modifiers } => {
                let name = special_key_name(code).ok_or_else(|| {
                    DesktopError::Backend(format!("unknown special key code {code}"))
                })?;
                let mut combination = modifier_args(modifiers);
                combination.push(name.to_string());
                self.run(vec!["key".to_string(), combination.join("+")])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::model::Modifiers;

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, _program: &str, args: &[String]) -> io::Result<()> {
            self.calls.lock().unwrap().push(args.to_vec());
            Ok(())
        }
    }

    fn input() -> (XdotoolInput, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner::default());
        (
            XdotoolInput::with_runner(Arc::clone(&runner) as Arc<dyn Runner>),
            runner,
        )
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[tokio::test]
    async fn movement_uses_a_relative_move() {
        let (input, runner) = input();
        input
            .pointer(PointerEvent::movement(3.4, -2.6))
            .await
            .unwrap();
        assert_eq!(
            runner.calls(),
            vec![args(&["mousemove_relative", "--", "3", "-3"])]
        );
    }

    #[tokio::test]
    async fn a_click_repeats_for_a_double_click() {
        let (input, runner) = input();
        input
            .pointer(PointerEvent::click(PointerButton::Right, 2))
            .await
            .unwrap();
        assert_eq!(
            runner.calls(),
            vec![args(&["click", "--repeat", "2", "3"])]
        );
    }

    #[tokio::test]
    async fn a_drag_presses_and_releases() {
        let (input, runner) = input();
        input
            .pointer(PointerEvent::button(PointerButton::Left, true))
            .await
            .unwrap();
        input
            .pointer(PointerEvent::button(PointerButton::Left, false))
            .await
            .unwrap();
        assert_eq!(
            runner.calls(),
            vec![args(&["mousedown", "1"]), args(&["mouseup", "1"])]
        );
    }

    #[tokio::test]
    async fn scrolling_down_uses_button_five() {
        let (input, runner) = input();
        input
            .pointer(PointerEvent::scroll(0.0, -3.0))
            .await
            .unwrap();
        assert_eq!(runner.calls(), vec![args(&["click", "--repeat", "3", "5"])]);
    }

    #[tokio::test]
    async fn a_special_key_maps_to_its_name() {
        let (input, runner) = input();
        input
            .key(KeyEvent::Special {
                code: 12,
                modifiers: Modifiers {
                    ctrl: true,
                    ..Modifiers::default()
                },
            })
            .await
            .unwrap();
        assert_eq!(runner.calls(), vec![args(&["key", "ctrl+Return"])]);
    }

    #[tokio::test]
    async fn text_with_modifiers_presses_them_around_the_text() {
        let (input, runner) = input();
        input
            .key(KeyEvent::Text {
                text: "-s".to_string(),
                modifiers: Modifiers {
                    ctrl: true,
                    ..Modifiers::default()
                },
            })
            .await
            .unwrap();
        assert_eq!(
            runner.calls(),
            vec![args(&["keydown", "ctrl", "type", "--", "-s", "keyup", "ctrl"])]
        );
    }

    #[test]
    fn plain_text_is_typed_without_modifiers() {
        assert_eq!(
            text_command("hello", Modifiers::default()),
            args(&["type", "hello"])
        );
    }

    #[test]
    fn several_modifiers_are_released_in_reverse() {
        assert_eq!(
            text_command(
                "a",
                Modifiers {
                    ctrl: true,
                    alt: true,
                    shift: true,
                    sup: true,
                }
            ),
            args(&[
                "keydown", "ctrl", "keydown", "alt", "keydown", "shift", "keydown", "super", "type",
                "a", "keyup", "super", "keyup", "shift", "keyup", "alt", "keyup", "ctrl",
            ])
        );
    }

    #[tokio::test]
    async fn an_unknown_special_key_is_refused() {
        let (input, runner) = input();
        let error = input
            .key(KeyEvent::Special {
                code: 200,
                modifiers: Modifiers::default(),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, DesktopError::Backend(_)));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn every_special_key_up_to_f12_has_a_name() {
        for code in 1..=32 {
            assert!(special_key_name(code).is_some(), "code {code}");
        }
        assert_eq!(special_key_name(0), None);
        assert_eq!(special_key_name(33), None);
    }
}
