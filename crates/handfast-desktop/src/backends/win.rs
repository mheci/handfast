//! Windows desktop backends.
//!
//! Every adapter either runs an in-box Windows executable by absolute path or
//! wraps a safe crate.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde::Deserialize;

use crate::backends::clipboard::{CommandOutput, CommandRunner, TokioRunner};
use crate::backends::shell::{Spawner, TokioSpawner};
use crate::error::{DesktopError, Result};
use crate::model::{
    ButtonAction, KeyEvent, MediaAction, Modifiers, Notification, PlaybackState, PointerButton,
    PointerEvent, PowerState, VolumeState,
};
use crate::traits::{Clipboard, Input, Lock, Media, Notifier, Power, Screensaver, Shell, Volume};

/// `%SystemRoot%\System32\<program>`, the only directory a peer-driven tool may
/// come from.
#[must_use]
pub fn system32(program: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join(program)
}

/// The in-box Windows PowerShell, used for toast, SMTC and battery calls.
#[must_use]
pub fn powershell() -> PathBuf {
    system32("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str().ok_or(DesktopError::Unsupported(
        "the Windows system path is not UTF-8",
    ))
}

fn failure(program: &Path, output: &CommandOutput) -> DesktopError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let code = match output.code {
        Some(code) => code.to_string(),
        None => "a signal".to_string(),
    };
    let name = program.display();
    let detail = if stderr.is_empty() {
        format!("{name} exited with {code}")
    } else {
        format!("{name} exited with {code}: {stderr}")
    };
    DesktopError::Backend(detail)
}

/// Session locking through `rundll32.exe`.
pub struct WinLock {
    runner: Arc<dyn CommandRunner>,
    program: PathBuf,
}

impl WinLock {
    /// Build a lock adapter that runs the in-box `rundll32.exe`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioRunner),
            program: system32("rundll32.exe"),
        }
    }

    /// Build a lock adapter over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn CommandRunner>, program: PathBuf) -> Self {
        Self { runner, program }
    }
}

impl Default for WinLock {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Lock for WinLock {
    async fn lock(&self) -> Result<()> {
        let program = path_str(&self.program)?;
        let output = self
            .runner
            .run(program, &["user32.dll,LockWorkStation"], None)
            .await
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    DesktopError::Unsupported("rundll32.exe is not installed")
                } else {
                    DesktopError::Backend(format!("failed to run {program}: {error}"))
                }
            })?;
        if !output.success {
            return Err(failure(&self.program, &output));
        }
        Ok(())
    }
}

/// URL opening through `explorer.exe`.
pub struct ExplorerShell {
    spawner: Arc<dyn Spawner>,
    program: PathBuf,
}

impl ExplorerShell {
    /// Build a shell adapter that runs the in-box `explorer.exe`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            spawner: Arc::new(TokioSpawner),
            program: system32("explorer.exe"),
        }
    }

    /// Build a shell adapter over a custom spawner.
    #[must_use]
    pub fn with_spawner(spawner: Arc<dyn Spawner>, program: PathBuf) -> Self {
        Self { spawner, program }
    }
}

impl Default for ExplorerShell {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Shell for ExplorerShell {
    async fn open_url(&self, url: &str) -> Result<()> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(DesktopError::Backend(
                "refusing to open a URL that is not http or https".to_string(),
            ));
        }
        let program = path_str(&self.program)?;
        self.spawner.spawn(program, &[url]).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                DesktopError::Unsupported("explorer.exe is not installed")
            } else {
                DesktopError::Backend(format!("failed to run {program}: {error}"))
            }
        })
    }
}

/// Clipboard operations. Tests substitute an in-memory fake.
pub trait ClipboardOps: Send + Sync {
    /// Read the clipboard text.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be opened or does not hold text.
    fn read(&self) -> std::result::Result<String, String>;

    /// Replace the clipboard text.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be opened.
    fn write(&self, text: &str) -> std::result::Result<(), String>;
}

/// [`ClipboardOps`] backed by `clipboard-win`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClipboard;

impl ClipboardOps for SystemClipboard {
    fn read(&self) -> std::result::Result<String, String> {
        clipboard_win::get_clipboard_string().map_err(|error| error.to_string())
    }

    fn write(&self, text: &str) -> std::result::Result<(), String> {
        clipboard_win::set_clipboard_string(text).map_err(|error| error.to_string())
    }
}

/// Clipboard access through `clipboard-win`.
pub struct WinClipboard {
    ops: Arc<dyn ClipboardOps>,
}

impl WinClipboard {
    /// Build a clipboard adapter over the real Windows clipboard.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ops: Arc::new(SystemClipboard),
        }
    }

    /// Build a clipboard adapter over a custom clipboard.
    #[must_use]
    pub fn with_ops(ops: Arc<dyn ClipboardOps>) -> Self {
        Self { ops }
    }

    async fn blocking<T: Send + 'static>(
        ops: Arc<dyn ClipboardOps>,
        op: impl FnOnce(&dyn ClipboardOps) -> std::result::Result<T, String> + Send + 'static,
    ) -> Result<T> {
        tokio::task::spawn_blocking(move || op(ops.as_ref()))
            .await
            .map_err(|error| DesktopError::Backend(format!("clipboard task failed: {error}")))?
            .map_err(DesktopError::Backend)
    }
}

impl Default for WinClipboard {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Clipboard for WinClipboard {
    async fn read_text(&self) -> Result<Option<String>> {
        let text = Self::blocking(Arc::clone(&self.ops), |ops| ops.read()).await?;
        if text.is_empty() {
            return Ok(None);
        }
        Ok(Some(text))
    }

    async fn write_text(&self, text: &str) -> Result<()> {
        let text = text.to_string();
        Self::blocking(Arc::clone(&self.ops), move |ops| ops.write(&text)).await
    }
}

/// A held keep-awake request. Releasing it ends the request.
pub trait Awake: Send {
    /// End the request.
    fn release(self: Box<Self>);
}

/// Starts one keep-awake request. Tests substitute a fake.
pub trait Keeper: Send + Sync {
    /// Keep the display and system awake.
    ///
    /// # Errors
    /// Fails when Windows refuses the request.
    fn keep_awake(&self, reason: &str) -> Result<Box<dyn Awake>>;
}

struct ThreadAwake {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Awake for ThreadAwake {
    fn release(mut self: Box<Self>) {
        self.release.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// [`Keeper`] backed by `keepawake`.
///
/// `SetThreadExecutionState` is per thread, so the request lives on its own
/// thread and is cleared there when the request ends.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadKeeper;

impl Keeper for ThreadKeeper {
    fn keep_awake(&self, reason: &str) -> Result<Box<dyn Awake>> {
        let (release, wait) = std::sync::mpsc::channel();
        let (ready, started) = std::sync::mpsc::channel();
        let reason = reason.to_string();
        let thread = std::thread::Builder::new()
            .name("handfast-keepawake".to_string())
            .spawn(move || {
                let awake = keepawake::Builder::default()
                    .display(true)
                    .idle(true)
                    .reason(reason)
                    .app_name("Handfast")
                    .create();
                match awake {
                    Ok(awake) => {
                        let _ = ready.send(Ok(()));
                        let _ = wait.recv();
                        drop(awake);
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                    }
                }
            })
            .map_err(|error| {
                DesktopError::Backend(format!("failed to start the keep-awake thread: {error}"))
            })?;
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Box::new(ThreadAwake {
                release: Some(release),
                thread: Some(thread),
            })),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(DesktopError::Backend(format!(
                    "failed to keep the display awake: {error}"
                )))
            }
            Err(_) => {
                let _ = thread.join();
                Err(DesktopError::Backend(
                    "the keep-awake thread did not start".to_string(),
                ))
            }
        }
    }
}

struct Active {
    holders: usize,
    awake: Box<dyn Awake>,
}

/// Screensaver inhibition through `keepawake`.
pub struct WinScreensaver {
    keeper: Arc<dyn Keeper>,
    active: tokio::sync::Mutex<Option<Active>>,
}

impl WinScreensaver {
    /// Build an inhibitor over the real Windows power API.
    #[must_use]
    pub fn new() -> Self {
        Self {
            keeper: Arc::new(ThreadKeeper),
            active: tokio::sync::Mutex::new(None),
        }
    }

    /// Build an inhibitor over a custom keeper.
    #[must_use]
    pub fn with_keeper(keeper: Arc<dyn Keeper>) -> Self {
        Self {
            keeper,
            active: tokio::sync::Mutex::new(None),
        }
    }
}

impl Default for WinScreensaver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Screensaver for WinScreensaver {
    async fn inhibit(&self, reason: &str) -> Result<()> {
        let mut state = self.active.lock().await;
        if let Some(active) = state.as_mut() {
            active.holders += 1;
            return Ok(());
        }
        let keeper = Arc::clone(&self.keeper);
        let reason = reason.to_string();
        let awake = tokio::task::spawn_blocking(move || keeper.keep_awake(&reason))
            .await
            .map_err(|error| DesktopError::Backend(format!("keep-awake task failed: {error}")))??;
        *state = Some(Active { holders: 1, awake });
        Ok(())
    }

    async fn release(&self) -> Result<()> {
        let mut state = self.active.lock().await;
        let Some(mut active) = state.take() else {
            return Ok(());
        };
        active.holders -= 1;
        if active.holders > 0 {
            *state = Some(active);
            return Ok(());
        }
        drop(state);
        tokio::task::spawn_blocking(move || active.awake.release())
            .await
            .map_err(|error| DesktopError::Backend(format!("keep-awake task failed: {error}")))
    }
}

/// Escape text for a toast XML text node.
fn escape_xml(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            character => escaped.push(character),
        }
    }
    escaped
}

fn toast_xml(notification: &Notification) -> String {
    let title = if notification.app_name.is_empty() {
        notification.title.clone()
    } else if notification.title.is_empty() {
        notification.app_name.clone()
    } else {
        format!("{} · {}", notification.app_name, notification.title)
    };
    if notification.text.is_empty() {
        format!(
            "<toast><visual><binding template=\"ToastText01\"><text id=\"1\">{}</text></binding></visual></toast>",
            escape_xml(&title)
        )
    } else {
        format!(
            "<toast><visual><binding template=\"ToastText02\"><text id=\"1\">{}</text><text id=\"2\">{}</text></binding></visual></toast>",
            escape_xml(&title),
            escape_xml(&notification.text)
        )
    }
}

fn toast_tag(id: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = u32::from(*chunk.get(1).unwrap_or(&0));
        let third = u32::from(*chunk.get(2).unwrap_or(&0));
        let value = (first << 16) | (second << 8) | third;
        encoded.push(ALPHABET[(value >> 18) as usize] as char);
        encoded.push(ALPHABET[((value >> 12) & 63) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

/// Encode a PowerShell script for `-EncodedCommand`, which expects base64 of
/// UTF-16LE text.
fn encoded_command(script: &str) -> String {
    let mut bytes = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    base64(&bytes)
}

fn powershell_args(script: &str) -> Vec<String> {
    vec![
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-EncodedCommand".to_string(),
        encoded_command(script),
    ]
}

async fn run_script(
    runner: &dyn CommandRunner,
    powershell: &Path,
    script: &str,
) -> Result<Vec<u8>> {
    let program = path_str(powershell)?;
    let args = powershell_args(script);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = runner.run(program, &args, None).await.map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            DesktopError::Unsupported("Windows PowerShell is not installed")
        } else {
            DesktopError::Backend(format!("failed to run {program}: {error}"))
        }
    })?;
    if !output.success {
        return Err(failure(powershell, &output));
    }
    Ok(output.stdout)
}

/// Desktop notifications through the in-box toast API.
pub struct WinNotifier {
    runner: Arc<dyn CommandRunner>,
    powershell: PathBuf,
    next_id: AtomicU64,
}

impl WinNotifier {
    /// Build a notifier that runs the in-box Windows PowerShell.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioRunner),
            powershell: powershell(),
            next_id: AtomicU64::new(0),
        }
    }

    /// Build a notifier over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn CommandRunner>, powershell: PathBuf) -> Self {
        Self {
            runner,
            powershell,
            next_id: AtomicU64::new(0),
        }
    }

    fn tag_for(&self, notification: &Notification) -> String {
        match notification.id.as_deref() {
            Some(id) => toast_tag(id),
            None => format!(
                "handfast-{:016x}",
                self.next_id.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }
}

impl Default for WinNotifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Notifier for WinNotifier {
    async fn notify(&self, notification: &Notification) -> Result<()> {
        let xml = base64(toast_xml(notification).as_bytes());
        let tag = base64(self.tag_for(notification).as_bytes());
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] | Out-Null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType=WindowsRuntime] | Out-Null
$document = New-Object Windows.Data.Xml.Dom.XmlDocument
$document.LoadXml([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{xml}')))
$toast = New-Object Windows.UI.Notifications.ToastNotification $document
$toast.Tag = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{tag}'))
$toast.Group = 'handfast'
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Handfast').Show($toast)
"#
        );
        run_script(self.runner.as_ref(), &self.powershell, &script).await?;
        Ok(())
    }

    async fn close(&self, id: &str) -> Result<()> {
        let tag = base64(toast_tag(id).as_bytes());
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] | Out-Null
$tag = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{tag}'))
[Windows.UI.Notifications.ToastNotificationManager]::History.Remove($tag, 'handfast', 'Handfast')
"#
        );
        run_script(self.runner.as_ref(), &self.powershell, &script).await?;
        Ok(())
    }
}

fn valid_player(player: &str) -> bool {
    !player.is_empty()
        && player.len() <= 128
        && player
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'!'))
}

const MEDIA_PREAMBLE: &str = r#"$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$asTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object { $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' })[0]
function Await($op, $type) { $task = $asTaskGeneric.MakeGenericMethod($type).Invoke($null, @($op)); $task.Wait(5000) | Out-Null; $task.Result }
$managerType = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media.Control, ContentType=WindowsRuntime]
$manager = Await ($managerType::RequestAsync()) $managerType
"#;

#[derive(Deserialize)]
struct PlayersReply {
    #[serde(default)]
    players: Vec<String>,
}

#[derive(Deserialize)]
struct PlaybackReply {
    ok: bool,
    #[serde(default)]
    player: String,
    #[serde(default)]
    playing: bool,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    position_ms: i64,
    #[serde(default)]
    length_ms: i64,
}

#[derive(Deserialize)]
struct OkReply {
    ok: bool,
}

// shortcut: one PowerShell process per SMTC call because the only safe wrapper
// crates are 0.1 and read-only; move to a safe control wrapper when one lands.
/// Media control through the System Media Transport Controls session manager.
pub struct WinMedia {
    runner: Arc<dyn CommandRunner>,
    powershell: PathBuf,
}

impl WinMedia {
    /// Build a media adapter that runs the in-box Windows PowerShell.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioRunner),
            powershell: powershell(),
        }
    }

    /// Build a media adapter over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn CommandRunner>, powershell: PathBuf) -> Self {
        Self { runner, powershell }
    }

    async fn json<T: for<'de> Deserialize<'de>>(&self, script: &str) -> Result<T> {
        let stdout = run_script(self.runner.as_ref(), &self.powershell, script).await?;
        serde_json::from_slice(&stdout).map_err(|error| {
            DesktopError::Backend(format!("Windows media returned invalid JSON: {error}"))
        })
    }
}

impl Default for WinMedia {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Media for WinMedia {
    async fn players(&self) -> Result<Vec<String>> {
        let script = format!(
            "{MEDIA_PREAMBLE}$players = @($manager.GetSessions() | ForEach-Object {{ $_.SourceAppUserModelId }} | Select-Object -Unique)\n@{{ players = $players }} | ConvertTo-Json -Compress\n"
        );
        let reply: PlayersReply = self.json(&script).await?;
        Ok(reply.players)
    }

    async fn playback(&self, player: &str) -> Result<Option<PlaybackState>> {
        if !valid_player(player) {
            return Err(DesktopError::Backend(
                "the media player name is not valid".to_string(),
            ));
        }
        let script = format!(
            r#"{MEDIA_PREAMBLE}$session = $manager.GetSessions() | Where-Object {{ $_.SourceAppUserModelId -eq '{player}' }} | Select-Object -First 1
if (-not $session) {{ '{{"ok":false}}'; exit 0 }}
$properties = Await ($session.TryGetMediaPropertiesAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties])
$info = $session.GetPlaybackInfo()
$timeline = $session.GetTimelineProperties()
$position = [long](($timeline.Position - $timeline.StartTime).TotalMilliseconds)
$length = [long](($timeline.EndTime - $timeline.StartTime).TotalMilliseconds)
@{{ ok = $true; player = '{player}'; playing = ($info.PlaybackStatus.ToString() -eq 'Playing'); title = $properties.Title; artist = $properties.Artist; position_ms = $position; length_ms = $length }} | ConvertTo-Json -Compress
"#
        );
        let reply: PlaybackReply = self.json(&script).await?;
        if !reply.ok {
            return Ok(None);
        }
        Ok(Some(PlaybackState {
            player: if reply.player.is_empty() {
                player.to_string()
            } else {
                reply.player
            },
            playing: reply.playing,
            title: reply.title.filter(|title| !title.is_empty()),
            artist: reply.artist.filter(|artist| !artist.is_empty()),
            position_ms: reply.position_ms,
            length_ms: reply.length_ms,
            volume: None,
        }))
    }

    async fn control(&self, player: &str, action: MediaAction) -> Result<()> {
        if !valid_player(player) {
            return Err(DesktopError::Backend(
                "the media player name is not valid".to_string(),
            ));
        }
        let method = match action {
            MediaAction::Play => "TryPlayAsync",
            MediaAction::Pause => "TryPauseAsync",
            MediaAction::PlayPause => "TryTogglePlayPauseAsync",
            MediaAction::Stop => "TryStopAsync",
            MediaAction::Next => "TrySkipNextAsync",
            MediaAction::Previous => "TrySkipPreviousAsync",
        };
        let script = format!(
            r#"{MEDIA_PREAMBLE}$session = $manager.GetSessions() | Where-Object {{ $_.SourceAppUserModelId -eq '{player}' }} | Select-Object -First 1
if (-not $session) {{ '{{"ok":false}}'; exit 0 }}
$ok = Await ($session.{method}()) ([bool])
@{{ ok = [bool]$ok }} | ConvertTo-Json -Compress
"#
        );
        let reply: OkReply = self.json(&script).await?;
        if !reply.ok {
            return Err(DesktopError::Backend(format!(
                "the media player {player} refused the request"
            )));
        }
        Ok(())
    }

    async fn set_volume(&self, player: &str, percent: u8) -> Result<()> {
        if !valid_player(player) {
            return Err(DesktopError::Backend(
                "the media player name is not valid".to_string(),
            ));
        }
        let level = f64::from(percent.min(100)) / 100.0;
        let script = format!(
            r#"{MEDIA_PREAMBLE}$session = $manager.GetSessions() | Where-Object {{ $_.SourceAppUserModelId -eq '{player}' }} | Select-Object -First 1
if (-not $session) {{ '{{"ok":false}}'; exit 0 }}
$ok = Await ($session.TryChangeVolumeAsync([double]{level:.2})) ([bool])
@{{ ok = [bool]$ok }} | ConvertTo-Json -Compress
"#
        );
        let reply: OkReply = self.json(&script).await?;
        if !reply.ok {
            return Err(DesktopError::Backend(format!(
                "the media player {player} refused the volume change"
            )));
        }
        Ok(())
    }
}

/// One audio endpoint. Tests substitute a fake.
pub trait Endpoint: Send + Sync {
    /// Read the current volume and mute state.
    ///
    /// # Errors
    /// Fails when the endpoint cannot be read.
    fn read(&self) -> std::result::Result<VolumeState, String>;

    /// Set the volume and mute state.
    ///
    /// # Errors
    /// Fails when the endpoint cannot be changed.
    fn write(&self, percent: u8, muted: bool) -> std::result::Result<(), String>;
}

/// [`Endpoint`] backed by the default WASAPI output device.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEndpoint;

impl Endpoint for SystemEndpoint {
    fn read(&self) -> std::result::Result<VolumeState, String> {
        use volumecontrol_core::AudioDevice as _;

        let device = volumecontrol_windows::AudioDevice::from_default()
            .map_err(|error| error.to_string())?;
        Ok(VolumeState {
            percent: device.get_vol().map_err(|error| error.to_string())?,
            muted: device.is_mute().map_err(|error| error.to_string())?,
        })
    }

    fn write(&self, percent: u8, muted: bool) -> std::result::Result<(), String> {
        use volumecontrol_core::AudioDevice as _;

        let device = volumecontrol_windows::AudioDevice::from_default()
            .map_err(|error| error.to_string())?;
        device.set_vol(percent).map_err(|error| error.to_string())?;
        device.set_mute(muted).map_err(|error| error.to_string())
    }
}

/// System output volume through WASAPI.
pub struct WinVolume {
    endpoint: Arc<dyn Endpoint>,
}

impl WinVolume {
    /// Build a volume adapter over the default Windows output device.
    #[must_use]
    pub fn new() -> Self {
        Self {
            endpoint: Arc::new(SystemEndpoint),
        }
    }

    /// Build a volume adapter over a custom endpoint.
    #[must_use]
    pub fn with_endpoint(endpoint: Arc<dyn Endpoint>) -> Self {
        Self { endpoint }
    }

    async fn blocking<T: Send + 'static>(
        endpoint: Arc<dyn Endpoint>,
        op: impl FnOnce(&dyn Endpoint) -> std::result::Result<T, String> + Send + 'static,
    ) -> Result<T> {
        tokio::task::spawn_blocking(move || op(endpoint.as_ref()))
            .await
            .map_err(|error| DesktopError::Backend(format!("volume task failed: {error}")))?
            .map_err(DesktopError::Backend)
    }
}

impl Default for WinVolume {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Volume for WinVolume {
    async fn get(&self) -> Result<VolumeState> {
        Self::blocking(Arc::clone(&self.endpoint), |endpoint| endpoint.read()).await
    }

    async fn set(&self, percent: u8, muted: bool) -> Result<()> {
        let percent = percent.min(100);
        Self::blocking(Arc::clone(&self.endpoint), move |endpoint| {
            endpoint.write(percent, muted)
        })
        .await
    }
}

#[derive(Deserialize)]
struct PowerReply {
    present: bool,
    #[serde(default)]
    percent: u8,
    #[serde(default)]
    charging: bool,
}

/// Host battery state through the in-box Windows PowerShell.
pub struct WinPower {
    runner: Arc<dyn CommandRunner>,
    powershell: PathBuf,
}

impl WinPower {
    /// Build a battery adapter that runs the in-box Windows PowerShell.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioRunner),
            powershell: powershell(),
        }
    }

    /// Build a battery adapter over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn CommandRunner>, powershell: PathBuf) -> Self {
        Self { runner, powershell }
    }
}

impl Default for WinPower {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Power for WinPower {
    async fn state(&self) -> Result<Option<PowerState>> {
        let script = r#"$battery = Get-CimInstance Win32_Battery | Select-Object -First 1
if (-not $battery) { '{"present":false}'; exit 0 }
$charging = $battery.BatteryStatus -ne 1 -and $battery.BatteryStatus -ne 4 -and $battery.BatteryStatus -ne 5
@{ present = $true; percent = [int]$battery.EstimatedChargeRemaining; charging = [bool]$charging } | ConvertTo-Json -Compress
"#;
        let stdout = run_script(self.runner.as_ref(), &self.powershell, script).await?;
        let reply: PowerReply = serde_json::from_slice(&stdout).map_err(|error| {
            DesktopError::Backend(format!("Windows power returned invalid JSON: {error}"))
        })?;
        if !reply.present {
            return Ok(None);
        }
        Ok(Some(PowerState {
            percent: reply.percent.min(100),
            charging: reply.charging,
        }))
    }
}

/// Injects pointer and keyboard input through `enigo`.
pub struct WinInput {
    enigo: std::sync::Mutex<Option<Enigo>>,
}

impl WinInput {
    /// Build an input adapter. The connection is created on first use.
    #[must_use]
    pub fn new() -> Self {
        Self {
            enigo: std::sync::Mutex::new(None),
        }
    }

    fn with_enigo<T>(&self, op: impl FnOnce(&mut Enigo) -> enigo::InputResult<T>) -> Result<T> {
        let mut guard = self
            .enigo
            .lock()
            .map_err(|_| DesktopError::Backend("the input lock is poisoned".to_string()))?;
        if guard.is_none() {
            *guard = Some(Enigo::new(&Settings::default()).map_err(|error| {
                DesktopError::Backend(format!(
                    "failed to initialize the Windows input backend: {error}"
                ))
            })?);
        }
        op(guard.as_mut().expect("the input backend was just created"))
            .map_err(|error| DesktopError::Backend(error.to_string()))
    }
}

impl Default for WinInput {
    fn default() -> Self {
        Self::new()
    }
}

fn modifier_keys(modifiers: Modifiers) -> Vec<Key> {
    let mut keys = Vec::new();
    if modifiers.ctrl {
        keys.push(Key::Control);
    }
    if modifiers.alt {
        keys.push(Key::Alt);
    }
    if modifiers.shift {
        keys.push(Key::Shift);
    }
    if modifiers.sup {
        keys.push(Key::Meta);
    }
    keys
}

fn special_key(code: u8) -> Option<Key> {
    Some(match code {
        1 => Key::Backspace,
        2 => Key::Tab,
        3 => Key::Return,
        4 => Key::LeftArrow,
        5 => Key::UpArrow,
        6 => Key::RightArrow,
        7 => Key::DownArrow,
        8 => Key::PageUp,
        9 => Key::PageDown,
        10 => Key::Home,
        11 => Key::End,
        12 => Key::Return,
        13 => Key::Delete,
        14 => Key::Escape,
        15 => Key::PrintScr,
        16 => Key::Scroll,
        17 => Key::Control,
        18 => Key::Alt,
        19 => Key::Shift,
        20 => Key::Meta,
        21 => Key::F1,
        22 => Key::F2,
        23 => Key::F3,
        24 => Key::F4,
        25 => Key::F5,
        26 => Key::F6,
        27 => Key::F7,
        28 => Key::F8,
        29 => Key::F9,
        30 => Key::F10,
        31 => Key::F11,
        32 => Key::F12,
        _ => return None,
    })
}

#[async_trait]
impl Input for WinInput {
    async fn pointer(&self, event: PointerEvent) -> Result<()> {
        self.with_enigo(|enigo| {
            if event.scroll {
                let dy = event.dy.round() as i32;
                if dy != 0 {
                    enigo.scroll(dy, Axis::Vertical)?;
                }
                let dx = event.dx.round() as i32;
                if dx != 0 {
                    enigo.scroll(dx, Axis::Horizontal)?;
                }
                Ok(())
            } else {
                enigo.move_mouse(
                    event.dx.round() as i32,
                    event.dy.round() as i32,
                    Coordinate::Rel,
                )
            }
        })
    }

    async fn button(&self, button: PointerButton, action: ButtonAction) -> Result<()> {
        let button = match button {
            PointerButton::Left => Button::Left,
            PointerButton::Middle => Button::Middle,
            PointerButton::Right => Button::Right,
        };
        self.with_enigo(|enigo| match action {
            ButtonAction::Click => enigo.button(button, Direction::Click),
            ButtonAction::DoubleClick => {
                enigo.button(button, Direction::Click)?;
                enigo.button(button, Direction::Click)
            }
            ButtonAction::Press => enigo.button(button, Direction::Press),
            ButtonAction::Release => enigo.button(button, Direction::Release),
        })
    }

    async fn key(&self, event: KeyEvent) -> Result<()> {
        let modifiers = match &event {
            KeyEvent::Text { modifiers, .. } | KeyEvent::Special { modifiers, .. } => *modifiers,
        };
        let special = match &event {
            KeyEvent::Special { code, .. } => Some(special_key(*code).ok_or(
                DesktopError::Unsupported("the special key is not supported"),
            )?),
            KeyEvent::Text { .. } => None,
        };
        self.with_enigo(|enigo| {
            let held = modifier_keys(modifiers);
            for key in &held {
                enigo.key(*key, Direction::Press)?;
            }
            let result = match &event {
                KeyEvent::Text { text, .. } => enigo.text(text),
                KeyEvent::Special { .. } => enigo.key(
                    special.expect("the special key was validated"),
                    Direction::Click,
                ),
            };
            for key in held.iter().rev() {
                let _ = enigo.key(*key, Direction::Release);
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    type Call = (String, Vec<String>, Option<Vec<u8>>);

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
            stdin: Option<&[u8]>,
        ) -> io::Result<CommandOutput> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|arg| arg.to_string()).collect(),
                stdin.map(<[u8]>::to_vec),
            ));
            self.output
                .lock()
                .unwrap()
                .take()
                .expect("the fake runner is called once")
        }
    }

    fn ok(stdout: &[u8]) -> io::Result<CommandOutput> {
        Ok(CommandOutput {
            success: true,
            code: Some(0),
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        })
    }

    fn decode_base64(text: &str) -> Vec<u8> {
        let mut output = Vec::new();
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for byte in text.bytes() {
            let value = match byte {
                b'A'..=b'Z' => u32::from(byte - b'A'),
                b'a'..=b'z' => u32::from(byte - b'a') + 26,
                b'0'..=b'9' => u32::from(byte - b'0') + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => break,
                other => panic!("unexpected base64 byte {other}"),
            };
            buffer = (buffer << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                output.push((buffer >> bits) as u8);
            }
        }
        output
    }

    fn script_of(call: &Call) -> String {
        assert_eq!(&call.1[4], "-EncodedCommand");
        let bytes = decode_base64(&call.1[5]);
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[tokio::test]
    async fn lock_runs_rundll32_from_system32() {
        let runner = FakeRunner::returning(ok(b""));
        let program = PathBuf::from(r"C:\Windows\System32\rundll32.exe");
        let lock = WinLock::with_runner(Arc::clone(&runner) as Arc<dyn CommandRunner>, program);

        lock.lock().await.unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, r"C:\Windows\System32\rundll32.exe");
        assert_eq!(calls[0].1, vec!["user32.dll,LockWorkStation"]);
    }

    #[tokio::test]
    async fn lock_reports_a_failure() {
        let runner = FakeRunner::returning(Ok(CommandOutput {
            success: false,
            code: Some(5),
            stdout: Vec::new(),
            stderr: b"access denied".to_vec(),
        }));
        let lock = WinLock::with_runner(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            PathBuf::from(r"C:\Windows\System32\rundll32.exe"),
        );

        let error = lock.lock().await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Backend(message) if message.contains("access denied")),
            "unexpected error: {error}"
        );
    }

    struct FakeSpawner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        failure: Option<io::ErrorKind>,
    }

    impl Spawner for FakeSpawner {
        fn spawn(&self, program: &str, args: &[&str]) -> io::Result<()> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|arg| arg.to_string()).collect(),
            ));
            match self.failure {
                Some(kind) => Err(io::Error::from(kind)),
                None => Ok(()),
            }
        }
    }

    fn shell(failure: Option<io::ErrorKind>) -> (ExplorerShell, Arc<FakeSpawner>) {
        let spawner = Arc::new(FakeSpawner {
            calls: Mutex::new(Vec::new()),
            failure,
        });
        let shell = ExplorerShell::with_spawner(
            Arc::clone(&spawner) as Arc<dyn Spawner>,
            PathBuf::from(r"C:\Windows\explorer.exe"),
        );
        (shell, spawner)
    }

    #[tokio::test]
    async fn shell_opens_http_and_https_only() {
        let (shell, spawner) = shell(None);

        shell.open_url("https://example.com/a b").await.unwrap();
        assert_eq!(
            spawner.calls.lock().unwrap().as_slice(),
            &[(
                r"C:\Windows\explorer.exe".to_string(),
                vec!["https://example.com/a b".to_string()]
            )]
        );

        for refused in ["file:///C:/Windows", "mailto:a@example.com", "--help", ""] {
            let error = shell.open_url(refused).await.unwrap_err();
            assert!(
                matches!(&error, DesktopError::Backend(message) if message.contains("refusing")),
                "unexpected error for {refused}: {error}"
            );
        }
        assert_eq!(spawner.calls.lock().unwrap().len(), 1);
    }

    struct FakeClipboard {
        text: Mutex<String>,
        reads: AtomicUsize,
        writes: Mutex<Vec<String>>,
    }

    impl ClipboardOps for FakeClipboard {
        fn read(&self) -> std::result::Result<String, String> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.text.lock().unwrap().clone())
        }

        fn write(&self, text: &str) -> std::result::Result<(), String> {
            self.writes.lock().unwrap().push(text.to_string());
            *self.text.lock().unwrap() = text.to_string();
            Ok(())
        }
    }

    #[tokio::test]
    async fn clipboard_round_trips_through_the_ops() {
        let ops = Arc::new(FakeClipboard {
            text: Mutex::new("before".to_string()),
            reads: AtomicUsize::new(0),
            writes: Mutex::new(Vec::new()),
        });
        let clipboard = WinClipboard::with_ops(Arc::clone(&ops) as Arc<dyn ClipboardOps>);

        assert_eq!(
            clipboard.read_text().await.unwrap(),
            Some("before".to_string())
        );
        clipboard.write_text("after").await.unwrap();
        assert_eq!(
            clipboard.read_text().await.unwrap(),
            Some("after".to_string())
        );
        assert_eq!(ops.writes.lock().unwrap().as_slice(), &["after"]);
    }

    #[tokio::test]
    async fn empty_clipboard_is_none() {
        let ops = Arc::new(FakeClipboard {
            text: Mutex::new(String::new()),
            reads: AtomicUsize::new(0),
            writes: Mutex::new(Vec::new()),
        });
        let clipboard = WinClipboard::with_ops(ops as Arc<dyn ClipboardOps>);

        assert_eq!(clipboard.read_text().await.unwrap(), None);
    }

    #[tokio::test]
    #[ignore = "writes the real clipboard"]
    async fn the_real_clipboard_round_trips() {
        let clipboard = WinClipboard::new();
        let previous = clipboard.read_text().await.unwrap();
        clipboard
            .write_text("handfast-clipboard-test")
            .await
            .unwrap();
        assert_eq!(
            clipboard.read_text().await.unwrap().as_deref(),
            Some("handfast-clipboard-test")
        );
        clipboard
            .write_text(previous.as_deref().unwrap_or(""))
            .await
            .unwrap();
    }

    struct FakeAwake {
        releases: Arc<AtomicUsize>,
    }

    impl Awake for FakeAwake {
        fn release(self: Box<Self>) {
            self.releases.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct FakeKeeper {
        calls: Mutex<Vec<String>>,
        releases: Arc<AtomicUsize>,
        failure: Mutex<Option<DesktopError>>,
    }

    impl Keeper for FakeKeeper {
        fn keep_awake(&self, reason: &str) -> Result<Box<dyn Awake>> {
            self.calls.lock().unwrap().push(reason.to_string());
            match self.failure.lock().unwrap().take() {
                Some(error) => Err(error),
                None => Ok(Box::new(FakeAwake {
                    releases: Arc::clone(&self.releases),
                })),
            }
        }
    }

    fn screensaver(failure: Option<DesktopError>) -> (WinScreensaver, Arc<FakeKeeper>) {
        let keeper = Arc::new(FakeKeeper {
            calls: Mutex::new(Vec::new()),
            releases: Arc::new(AtomicUsize::new(0)),
            failure: Mutex::new(failure),
        });
        (
            WinScreensaver::with_keeper(Arc::clone(&keeper) as Arc<dyn Keeper>),
            keeper,
        )
    }

    #[tokio::test]
    async fn screensaver_is_refcounted() {
        let (screensaver, keeper) = screensaver(None);

        screensaver.release().await.unwrap();
        screensaver.inhibit("first").await.unwrap();
        screensaver.inhibit("second").await.unwrap();
        assert_eq!(keeper.calls.lock().unwrap().as_slice(), &["first"]);

        screensaver.release().await.unwrap();
        assert_eq!(keeper.releases.load(Ordering::SeqCst), 0);

        screensaver.release().await.unwrap();
        assert_eq!(keeper.releases.load(Ordering::SeqCst), 1);

        screensaver.release().await.unwrap();
        assert_eq!(keeper.releases.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn screensaver_failure_is_retried() {
        let (screensaver, keeper) = screensaver(Some(DesktopError::Backend("no".to_string())));

        assert!(screensaver.inhibit("first").await.is_err());
        screensaver.inhibit("second").await.unwrap();

        assert_eq!(
            keeper.calls.lock().unwrap().as_slice(),
            &["first", "second"]
        );
    }

    #[test]
    fn toast_xml_escapes_text_and_picks_the_template() {
        let mut notification = Notification::new("App", "A & B", "");
        notification.id = Some("device/1".to_string());
        let xml = toast_xml(&notification);
        assert!(xml.contains("ToastText01"));
        assert!(xml.contains("App · A &amp; B"));

        notification.text = "<hello>".to_string();
        let xml = toast_xml(&notification);
        assert!(xml.contains("ToastText02"));
        assert!(xml.contains("&lt;hello&gt;"));
    }

    #[test]
    fn toast_tags_are_stable_and_short() {
        let first = toast_tag("device/1");
        let second = toast_tag("device/1");
        assert_eq!(first, second);
        assert_eq!(first.len(), 16);
        assert_ne!(first, toast_tag("device/2"));
    }

    #[tokio::test]
    async fn notifier_uses_an_encoded_powershell_script() {
        let runner = FakeRunner::returning(ok(b""));
        let notifier = WinNotifier::with_runner(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        );
        let mut notification = Notification::new("Messages", "Ada", "See you at six");
        notification.id = Some("device/1".to_string());

        notifier.notify(&notification).await.unwrap();

        let calls = runner.calls();
        assert_eq!(
            calls[0].0,
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
        );
        assert_eq!(
            &calls[0].1[0..4],
            &[
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass"
            ]
        );
        let script = script_of(&calls[0]);
        assert!(script.contains("CreateToastNotifier('Handfast')"));
        assert!(script.contains(&base64(b"Messages \xc2\xb7 Ada")));
    }

    #[tokio::test]
    async fn notifier_close_removes_the_tag() {
        let runner = FakeRunner::returning(ok(b""));
        let notifier = WinNotifier::with_runner(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        );

        notifier.close("device/1").await.unwrap();

        let calls = runner.calls();
        let script = script_of(&calls[0]);
        assert!(script.contains("History.Remove($tag, 'handfast', 'Handfast')"));
    }

    #[test]
    fn media_player_names_are_validated() {
        assert!(valid_player("308046B0AF4A39CB"));
        assert!(valid_player("spotify.exe"));
        assert!(valid_player(
            "Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"
        ));
        assert!(!valid_player(""));
        assert!(!valid_player("a'b"));
        assert!(!valid_player("a b"));
        assert!(!valid_player(&"a".repeat(129)));
    }

    #[tokio::test]
    async fn media_players_parse_the_session_list() {
        let runner = FakeRunner::returning(ok(br#"{"players":["chrome","spotify"]}"#));
        let media = WinMedia::with_runner(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        );

        assert_eq!(
            media.players().await.unwrap(),
            vec!["chrome".to_string(), "spotify".to_string()]
        );
        let script = script_of(&runner.calls()[0]);
        assert!(script.contains("GetSessions()"));
    }

    #[tokio::test]
    async fn media_playback_maps_the_state() {
        let runner = FakeRunner::returning(ok(
            br#"{"ok":true,"player":"chrome","playing":true,"title":"Song","artist":"Band","position_ms":1200,"length_ms":300000}"#,
        ));
        let media = WinMedia::with_runner(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        );

        let state = media.playback("chrome").await.unwrap().unwrap();
        assert_eq!(state.player, "chrome");
        assert!(state.playing);
        assert_eq!(state.title.as_deref(), Some("Song"));
        assert_eq!(state.artist.as_deref(), Some("Band"));
        assert_eq!(state.position_ms, 1200);
        assert_eq!(state.length_ms, 300_000);
        assert_eq!(state.volume, None);
    }

    #[tokio::test]
    async fn media_playback_missing_player_is_none() {
        let runner = FakeRunner::returning(ok(br#"{"ok":false}"#));
        let media = WinMedia::with_runner(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        );

        assert_eq!(media.playback("chrome").await.unwrap(), None);
    }

    #[tokio::test]
    async fn media_control_maps_each_action() {
        for (action, method) in [
            (MediaAction::Play, "TryPlayAsync"),
            (MediaAction::Pause, "TryPauseAsync"),
            (MediaAction::PlayPause, "TryTogglePlayPauseAsync"),
            (MediaAction::Stop, "TryStopAsync"),
            (MediaAction::Next, "TrySkipNextAsync"),
            (MediaAction::Previous, "TrySkipPreviousAsync"),
        ] {
            let runner = FakeRunner::returning(ok(br#"{"ok":true}"#));
            let media = WinMedia::with_runner(
                Arc::clone(&runner) as Arc<dyn CommandRunner>,
                PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
            );

            media.control("chrome", action).await.unwrap();

            let script = script_of(&runner.calls()[0]);
            assert!(script.contains(method), "missing {method}");
        }
    }

    #[tokio::test]
    async fn media_refuses_a_refused_action() {
        let runner = FakeRunner::returning(ok(br#"{"ok":false}"#));
        let media = WinMedia::with_runner(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        );

        assert!(media.control("chrome", MediaAction::Play).await.is_err());
    }

    #[tokio::test]
    async fn media_volume_clamps_and_formats_the_level() {
        let runner = FakeRunner::returning(ok(br#"{"ok":true}"#));
        let media = WinMedia::with_runner(
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
            PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        );

        media.set_volume("chrome", 250).await.unwrap();

        let script = script_of(&runner.calls()[0]);
        assert!(script.contains("TryChangeVolumeAsync([double]1.00)"));
    }

    struct FakeEndpoint {
        state: Mutex<VolumeState>,
        writes: Mutex<Vec<(u8, bool)>>,
        failure: Option<String>,
    }

    impl Endpoint for FakeEndpoint {
        fn read(&self) -> std::result::Result<VolumeState, String> {
            match &self.failure {
                Some(error) => Err(error.clone()),
                None => Ok(*self.state.lock().unwrap()),
            }
        }

        fn write(&self, percent: u8, muted: bool) -> std::result::Result<(), String> {
            self.writes.lock().unwrap().push((percent, muted));
            self.state.lock().unwrap().percent = percent;
            self.state.lock().unwrap().muted = muted;
            Ok(())
        }
    }

    #[tokio::test]
    async fn volume_reads_and_writes_the_endpoint() {
        let endpoint = Arc::new(FakeEndpoint {
            state: Mutex::new(VolumeState {
                percent: 40,
                muted: true,
            }),
            writes: Mutex::new(Vec::new()),
            failure: None,
        });
        let volume = WinVolume::with_endpoint(Arc::clone(&endpoint) as Arc<dyn Endpoint>);

        assert_eq!(
            volume.get().await.unwrap(),
            VolumeState {
                percent: 40,
                muted: true
            }
        );
        volume.set(150, false).await.unwrap();
        assert_eq!(endpoint.writes.lock().unwrap().as_slice(), &[(100, false)]);
    }

    #[test]
    fn special_keys_map_to_enigo_keys() {
        assert_eq!(special_key(1), Some(Key::Backspace));
        assert_eq!(special_key(4), Some(Key::LeftArrow));
        assert_eq!(special_key(12), Some(Key::Return));
        assert_eq!(special_key(21), Some(Key::F1));
        assert_eq!(special_key(32), Some(Key::F12));
        assert_eq!(special_key(33), None);
    }

    #[test]
    fn modifiers_are_held_in_a_fixed_order() {
        let keys = modifier_keys(Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
            sup: true,
        });

        assert_eq!(keys, vec![Key::Control, Key::Alt, Key::Shift, Key::Meta]);
    }

    #[tokio::test]
    async fn volume_errors_are_backend_errors() {
        let endpoint = Arc::new(FakeEndpoint {
            state: Mutex::new(VolumeState::default()),
            writes: Mutex::new(Vec::new()),
            failure: Some("no endpoint".to_string()),
        });
        let volume = WinVolume::with_endpoint(endpoint as Arc<dyn Endpoint>);

        let error = volume.get().await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Backend(message) if message.contains("no endpoint")),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    #[ignore = "reads the real Windows media sessions"]
    async fn the_real_media_backend_lists_and_reads_players() {
        let media = WinMedia::new();
        let players = media.players().await.unwrap();
        println!("players: {players:?}");
        if let Some(player) = players.first() {
            let state = media.playback(player).await.unwrap();
            println!("{player}: {state:?}");
        }
    }

    #[tokio::test]
    #[ignore = "shows a real Windows toast"]
    async fn the_real_notifier_shows_and_closes_a_toast() {
        let notifier = WinNotifier::new();
        let mut notification =
            Notification::new("Handfast", "Backend test", "The Windows toast path works");
        notification.id = Some("handfast-selftest".to_string());
        notifier.notify(&notification).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        notifier.close("handfast-selftest").await.unwrap();
    }

    #[tokio::test]
    #[ignore = "reads and rewrites the real output volume"]
    async fn the_real_volume_backend_round_trips() {
        let volume = WinVolume::new();
        let state = volume.get().await.unwrap();
        println!("volume: {state:?}");
        volume.set(state.percent, state.muted).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "holds a real keep-awake request"]
    async fn the_real_screensaver_inhibits_and_releases() {
        let screensaver = WinScreensaver::new();
        screensaver.inhibit("Handfast backend test").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        screensaver.release().await.unwrap();
    }
}
