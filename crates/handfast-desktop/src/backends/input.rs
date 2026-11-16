//! Pointer and keyboard injection through `ydotool` and `wtype`, plus a
//! virtual drawing tablet through the Linux `uinput` API.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;

use crate::backends::clipboard::{CommandOutput, CommandRunner, TokioRunner};
use crate::error::{DesktopError, Result};
#[cfg(target_os = "linux")]
use crate::model::PenTool;
use crate::model::{ButtonAction, KeyEvent, Modifiers, PenEvent, PointerButton, PointerEvent};
use crate::traits::Input;
#[cfg(target_os = "linux")]
use evdev::uinput::VirtualDevice;

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
    /// The uinput virtual tablet of the active digitizer session.
    #[cfg(target_os = "linux")]
    tablet: tokio::sync::Mutex<Option<evdev::uinput::VirtualDevice>>,
}

impl WaylandInput {
    /// Build an input adapter that runs the session tools.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioRunner),
            #[cfg(target_os = "linux")]
            tablet: tokio::sync::Mutex::new(None),
        }
    }

    /// Build an input adapter over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            runner,
            #[cfg(target_os = "linux")]
            tablet: tokio::sync::Mutex::new(None),
        }
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

/// The uinput pressure range for the virtual tablet, as upstream KDE Connect.
#[cfg(target_os = "linux")]
const PEN_PRESSURE_MAX: i32 = 1023;

/// Scale a pen pressure of 0-1 onto the tablet pressure range.
#[cfg(target_os = "linux")]
fn pressure_code(pressure: f64) -> i32 {
    (pressure.clamp(0.0, 1.0) * f64::from(PEN_PRESSURE_MAX)) as i32
}

/// The evdev events that set the full tool state of one pen event.
///
/// `touching: false` releases the touch button and zeroes pressure, so a lift
/// never reports stale force.
#[cfg(target_os = "linux")]
fn pen_events(event: &PenEvent) -> Vec<evdev::InputEvent> {
    use evdev::{AbsoluteAxisCode, AbsoluteAxisEvent, KeyCode, KeyEvent};

    let (pen, rubber) = match event.tool {
        PenTool::Pen => (1, 0),
        PenTool::Rubber => (0, 1),
    };
    let pressure = if event.touching {
        pressure_code(event.pressure)
    } else {
        0
    };
    vec![
        KeyEvent::new(KeyCode::BTN_TOOL_PEN, pen).into(),
        KeyEvent::new(KeyCode::BTN_TOOL_RUBBER, rubber).into(),
        KeyEvent::new(KeyCode::BTN_TOUCH, i32::from(event.touching)).into(),
        AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_X, event.x).into(),
        AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_Y, event.y).into(),
        AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_PRESSURE, pressure).into(),
    ]
}

/// Create the uinput virtual tablet for one digitizer session.
///
/// Mirrors upstream KDE Connect: `INPUT_PROP_DIRECT`, the pen and touch
/// buttons, and absolute axes over the session space with a 0-1023 pressure
/// range. Opening `/dev/uinput` fails on hosts without it.
#[cfg(target_os = "linux")]
fn build_tablet(
    width: i32,
    height: i32,
    resolution_x: i32,
    resolution_y: i32,
) -> Result<VirtualDevice> {
    use evdev::{
        AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, InputId, KeyCode, PropType,
        UinputAbsSetup,
    };

    let keys: AttributeSet<KeyCode> = [
        KeyCode::BTN_TOUCH,
        KeyCode::BTN_TOOL_PEN,
        KeyCode::BTN_TOOL_RUBBER,
    ]
    .into_iter()
    .collect();
    let properties: AttributeSet<PropType> =
        [PropType::DIRECT, PropType::POINTER].into_iter().collect();
    let open_error = |error: io::Error| {
        if error.kind() == io::ErrorKind::NotFound {
            DesktopError::Unsupported(
                "/dev/uinput is not available; pen injection needs the uinput module",
            )
        } else {
            DesktopError::Backend(format!("failed to open /dev/uinput: {error}"))
        }
    };
    let setup_error = |error: io::Error| {
        DesktopError::Backend(format!("failed to configure the tablet: {error}"))
    };
    let builder = VirtualDevice::builder()
        .map_err(open_error)?
        .name(b"Handfast virtual drawing tablet")
        .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x1234, 0x5678, 1))
        .with_keys(&keys)
        .map_err(setup_error)?
        .with_properties(&properties)
        .map_err(setup_error)?;
    builder
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_X,
            AbsInfo::new(0, 0, width - 1, 0, 0, resolution_x),
        ))
        .map_err(setup_error)?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            AbsInfo::new(0, 0, height - 1, 0, 0, resolution_y),
        ))
        .map_err(setup_error)?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_PRESSURE,
            AbsInfo::new(0, 0, PEN_PRESSURE_MAX, 0, 0, 0),
        ))
        .map_err(setup_error)?
        .build()
        .map_err(|error| DesktopError::Backend(format!("failed to create the tablet: {error}")))
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

    async fn start_pen_session(
        &self,
        width: i32,
        height: i32,
        resolution_x: i32,
        resolution_y: i32,
    ) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let tablet = build_tablet(width, height, resolution_x, resolution_y)?;
            *self.tablet.lock().await = Some(tablet);
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (width, height, resolution_x, resolution_y);
            Err(DesktopError::Unsupported(
                "pen injection needs /dev/uinput, which only exists on Linux",
            ))
        }
    }

    async fn end_pen_session(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            // Dropping the device closes /dev/uinput and removes the tablet.
            *self.tablet.lock().await = None;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(DesktopError::Unsupported(
                "pen injection needs /dev/uinput, which only exists on Linux",
            ))
        }
    }

    async fn pen(&self, event: PenEvent) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let mut tablet = self.tablet.lock().await;
            let Some(device) = tablet.as_mut() else {
                return Err(DesktopError::Unsupported("no pen session is active"));
            };
            let events = pen_events(&event);
            device.emit(&events).map_err(|error| {
                DesktopError::Backend(format!("failed to emit pen event: {error}"))
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = event;
            Err(DesktopError::Unsupported(
                "pen injection needs /dev/uinput, which only exists on Linux",
            ))
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

    #[cfg(target_os = "linux")]
    fn pen_event(
        x: i32,
        y: i32,
        pressure: f64,
        touching: bool,
        tool: PenTool,
    ) -> crate::model::PenEvent {
        crate::model::PenEvent {
            x,
            y,
            pressure,
            touching,
            tool,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pen_events_map_the_full_tool_state() {
        use evdev::{AbsoluteAxisCode, KeyCode};

        let events = pen_events(&pen_event(7, 9, 0.5, true, PenTool::Pen));
        let summary: Vec<(u16, u16, i32)> = events
            .iter()
            .map(|event| (event.event_type().0, event.code(), event.value()))
            .collect();
        // type, code, value: EV_KEY=1, EV_ABS=3.
        assert_eq!(
            summary,
            vec![
                (1, KeyCode::BTN_TOOL_PEN.0, 1),
                (1, KeyCode::BTN_TOOL_RUBBER.0, 0),
                (1, KeyCode::BTN_TOUCH.0, 1),
                (3, AbsoluteAxisCode::ABS_X.0, 7),
                (3, AbsoluteAxisCode::ABS_Y.0, 9),
                (3, AbsoluteAxisCode::ABS_PRESSURE.0, 511),
            ]
        );

        let events = pen_events(&pen_event(0, 0, 0.8, false, PenTool::Rubber));
        let summary: Vec<(u16, i32)> = events
            .iter()
            .map(|event| (event.code(), event.value()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (KeyCode::BTN_TOOL_PEN.0, 0),
                (KeyCode::BTN_TOOL_RUBBER.0, 1),
                (KeyCode::BTN_TOUCH.0, 0),
                (AbsoluteAxisCode::ABS_X.0, 0),
                (AbsoluteAxisCode::ABS_Y.0, 0),
                // A lift zeroes pressure even when the packet carried one.
                (AbsoluteAxisCode::ABS_PRESSURE.0, 0),
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pressure_code_clamps_the_range() {
        assert_eq!(pressure_code(0.0), 0);
        assert_eq!(pressure_code(1.0), PEN_PRESSURE_MAX);
        assert_eq!(pressure_code(1.5), PEN_PRESSURE_MAX);
        assert_eq!(pressure_code(-0.5), 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn pen_without_a_session_is_unsupported() {
        let input = WaylandInput::new();
        let error = input
            .pen(pen_event(0, 0, 0.5, true, PenTool::Pen))
            .await
            .unwrap_err();
        assert!(matches!(error, DesktopError::Unsupported(_)));

        // Ending a session that was never started stays a no-op.
        input.end_pen_session().await.unwrap();
    }
}
