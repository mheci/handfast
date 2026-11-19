//! In-memory desktop backend for contract tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::files::{FileLimits, Files, FilesError, LocalFiles};
use crate::model::{
    CallEvent, ContactRecord, KeyEvent, MediaAction, Notification, PlaybackState, PointerEvent,
    VirtualDisplay, VolumeState,
};
use crate::traits::{
    AudioSink, Battery, BatteryStatus, Clipboard, Contacts, Display, Input, Lock, Media, Notifier,
    Presenter, Screensaver, Shell, Telephony, Volume,
};
use crate::Desktop;

/// Recorded synthetic input.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// Pointer movement or scroll.
    Pointer(PointerEvent),
    /// Keyboard event.
    Key(KeyEvent),
    /// Absolute pointer position, as a screen fraction, and whether the
    /// primary button was down for the sample.
    Absolute { x: f64, y: f64, pressed: bool },
}

/// Counts lock calls made through the mock desktop.
#[derive(Debug, Default)]
pub struct MockLock {
    calls: AtomicUsize,
}

impl MockLock {
    /// Number of lock calls recorded.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Lock for MockLock {
    async fn lock(&self) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct MockState {
    clipboard: Mutex<Option<String>>,
    notifications: Mutex<Vec<Notification>>,
    volume: Mutex<VolumeState>,
    input: Mutex<Vec<InputEvent>>,
    shell_urls: Mutex<Vec<String>>,
    inhibited: Mutex<bool>,
    media_actions: Mutex<Vec<(String, MediaAction)>>,
    battery: Mutex<BatteryStatus>,
    presentations: Mutex<Vec<Option<(f32, f32)>>>,
    calls: Mutex<Vec<CallEvent>>,
    contacts: Mutex<Vec<ContactRecord>>,
    display: Mutex<Option<VirtualDisplay>>,
}

/// A desktop backend that records every operation.
#[derive(Clone)]
pub struct MockDesktop {
    state: Arc<MockState>,
    desktop: Desktop,
    files_root: Option<Arc<tempfile::TempDir>>,
    lock: Arc<MockLock>,
}

impl MockDesktop {
    /// Build a mock desktop with every adapter present and no files adapter.
    #[must_use]
    pub fn new() -> Self {
        let state = Arc::new(MockState::default());
        let backend = Arc::new(MockBackend(Arc::clone(&state)));
        let lock = Arc::new(MockLock::default());
        let desktop = Desktop {
            clipboard: Some(Arc::clone(&backend) as Arc<dyn Clipboard>),
            notifier: Some(Arc::clone(&backend) as Arc<dyn Notifier>),
            media: Some(Arc::clone(&backend) as Arc<dyn Media>),
            volume: Some(Arc::clone(&backend) as Arc<dyn Volume>),
            input: Some(Arc::clone(&backend) as Arc<dyn Input>),
            screensaver: Some(Arc::clone(&backend) as Arc<dyn Screensaver>),
            shell: Some(Arc::clone(&backend) as Arc<dyn Shell>),
            files: None,
            lock: Some(Arc::clone(&lock) as Arc<dyn Lock>),
            battery: Some(Arc::clone(&backend) as Arc<dyn Battery>),
            presenter: Some(Arc::clone(&backend) as Arc<dyn Presenter>),
            telephony: Some(Arc::clone(&backend) as Arc<dyn Telephony>),
            contacts: Some(Arc::clone(&backend) as Arc<dyn Contacts>),
            display: Some(Arc::clone(&backend) as Arc<dyn Display>),
        };
        Self {
            state,
            desktop,
            files_root: None,
            lock,
        }
    }

    /// Build a mock desktop with every adapter plus a [`LocalFiles`] adapter.
    ///
    /// The adapter writes into a temp directory owned by the mock.
    ///
    /// # Errors
    /// Fails when the temp directory or the incoming root cannot be created.
    pub fn with_files() -> std::result::Result<Self, FilesError> {
        let temp = tempfile::TempDir::new()?;
        let root = temp.path().join("incoming");
        let files = Arc::new(LocalFiles::new(&root, FileLimits::default())?);
        let mut mock = Self::new();
        mock.desktop.files = Some(files as Arc<dyn Files>);
        mock.files_root = Some(Arc::new(temp));
        Ok(mock)
    }

    /// Borrow the composed desktop.
    #[must_use]
    pub fn desktop(&self) -> Desktop {
        self.desktop.clone()
    }

    /// Root of the mock incoming directory, when a files adapter is installed.
    #[must_use]
    pub fn incoming_dir(&self) -> Option<PathBuf> {
        self.files_root
            .as_ref()
            .map(|temp| temp.path().join("incoming"))
    }

    /// Regular files saved under the mock incoming root, sorted by path.
    #[must_use]
    pub fn saved_files(&self) -> Vec<PathBuf> {
        let Some(root) = self.incoming_dir() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .map(|entry| entry.path())
            .collect();
        paths.sort();
        paths
    }

    /// Last clipboard text written by a plugin.
    #[must_use]
    pub fn clipboard(&self) -> Option<String> {
        self.state
            .clipboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Notifications delivered so far.
    #[must_use]
    pub fn notifications(&self) -> Vec<Notification> {
        self.state
            .notifications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Current mock volume.
    #[must_use]
    pub fn volume(&self) -> VolumeState {
        *self
            .state
            .volume
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Synthetic input recorded so far.
    #[must_use]
    pub fn input_events(&self) -> Vec<InputEvent> {
        self.state
            .input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// URLs opened through the shell adapter.
    #[must_use]
    pub fn shell_urls(&self) -> Vec<String> {
        self.state
            .shell_urls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Media actions sent so far.
    #[must_use]
    pub fn media_actions(&self) -> Vec<(String, MediaAction)> {
        self.state
            .media_actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether the screensaver is inhibited.
    #[must_use]
    pub fn inhibited(&self) -> bool {
        *self
            .state
            .inhibited
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Current mock battery state.
    #[must_use]
    pub fn battery(&self) -> BatteryStatus {
        *self
            .state
            .battery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replace the mock battery state.
    pub fn set_battery(&self, status: BatteryStatus) {
        *self
            .state
            .battery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = status;
    }

    /// Presentation view calls: `Some((x, y))` for a move, `None` for a stop.
    #[must_use]
    pub fn presentations(&self) -> Vec<Option<(f32, f32)>> {
        self.state
            .presentations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Lock calls recorded by the mock lock adapter.
    #[must_use]
    pub fn lock_calls(&self) -> usize {
        self.lock.calls()
    }

    /// Queue a telephony event for the next poll.
    pub fn push_call(&self, event: CallEvent) {
        self.state
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }

    /// Replace the mock address book.
    pub fn set_contacts(&self, records: Vec<ContactRecord>) {
        *self
            .state
            .contacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = records;
    }

    /// Geometry of the mock virtual display, when one was enabled.
    #[must_use]
    pub fn virtual_display(&self) -> Option<VirtualDisplay> {
        *self
            .state
            .display
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for MockDesktop {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
struct MockBackend(Arc<MockState>);

impl MockBackend {
    fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> Result<std::sync::MutexGuard<'a, T>> {
        mutex
            .lock()
            .map_err(|_| DesktopError::Backend("mock lock poisoned".to_string()))
    }
}

#[async_trait]
impl Clipboard for MockBackend {
    async fn read_text(&self) -> Result<Option<String>> {
        Ok(self.lock(&self.0.clipboard)?.clone())
    }

    async fn write_text(&self, text: &str) -> Result<()> {
        *self.lock(&self.0.clipboard)? = Some(text.to_string());
        Ok(())
    }
}

#[async_trait]
impl Notifier for MockBackend {
    async fn notify(&self, notification: &Notification) -> Result<()> {
        self.lock(&self.0.notifications)?.push(notification.clone());
        Ok(())
    }

    async fn close(&self, id: &str) -> Result<()> {
        self.lock(&self.0.notifications)?
            .retain(|notification| notification.id.as_deref() != Some(id));
        Ok(())
    }
}

#[async_trait]
impl Media for MockBackend {
    async fn players(&self) -> Result<Vec<String>> {
        Ok(vec!["Mock Player".to_string()])
    }

    async fn playback(&self, player: &str) -> Result<Option<PlaybackState>> {
        Ok(Some(PlaybackState {
            player: player.to_string(),
            playing: false,
            title: Some("Mock title".to_string()),
            artist: Some("Mock artist".to_string()),
            position_ms: 0,
            length_ms: 60_000,
            volume: Some(50),
        }))
    }

    async fn control(&self, player: &str, action: MediaAction) -> Result<()> {
        self.lock(&self.0.media_actions)?
            .push((player.to_string(), action));
        Ok(())
    }

    async fn set_volume(&self, _player: &str, _percent: u8) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Volume for MockBackend {
    async fn get(&self) -> Result<VolumeState> {
        Ok(*self.lock(&self.0.volume)?)
    }

    async fn set(&self, percent: u8, muted: bool) -> Result<()> {
        *self.lock(&self.0.volume)? = VolumeState {
            percent: percent.min(100),
            muted,
        };
        Ok(())
    }

    async fn sinks(&self) -> Result<Vec<AudioSink>> {
        let state = *self.lock(&self.0.volume)?;
        Ok(vec![AudioSink {
            name: "default".to_string(),
            description: "System volume".to_string(),
            volume: i64::from(state.percent),
            max_volume: 100,
            muted: state.muted,
            enabled: true,
        }])
    }

    async fn set_sink(&self, name: &str, volume: Option<i64>, muted: Option<bool>) -> Result<()> {
        if name != "default" {
            return Err(DesktopError::Backend(format!("unknown audio sink {name}")));
        }
        let mut state = self.lock(&self.0.volume)?;
        if let Some(volume) = volume {
            state.percent = u8::try_from(volume.clamp(0, 100)).unwrap_or(100);
        }
        if let Some(muted) = muted {
            state.muted = muted;
        }
        Ok(())
    }
}

#[async_trait]
impl Input for MockBackend {
    async fn pointer(&self, event: PointerEvent) -> Result<()> {
        self.lock(&self.0.input)?.push(InputEvent::Pointer(event));
        Ok(())
    }

    async fn key(&self, event: KeyEvent) -> Result<()> {
        self.lock(&self.0.input)?.push(InputEvent::Key(event));
        Ok(())
    }

    async fn pointer_absolute(&self, x: f64, y: f64, pressed: bool) -> Result<()> {
        self.lock(&self.0.input)?
            .push(InputEvent::Absolute { x, y, pressed });
        Ok(())
    }
}

#[async_trait]
impl Screensaver for MockBackend {
    async fn inhibit(&self, _reason: &str) -> Result<()> {
        *self.lock(&self.0.inhibited)? = true;
        Ok(())
    }

    async fn release(&self) -> Result<()> {
        *self.lock(&self.0.inhibited)? = false;
        Ok(())
    }
}

#[async_trait]
impl Battery for MockBackend {
    async fn status(&self) -> Result<BatteryStatus> {
        Ok(*self.lock(&self.0.battery)?)
    }
}

#[async_trait]
impl Presenter for MockBackend {
    async fn present(&self, x: f32, y: f32) -> Result<()> {
        self.lock(&self.0.presentations)?.push(Some((x, y)));
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.lock(&self.0.presentations)?.push(None);
        Ok(())
    }
}

#[async_trait]
impl Telephony for MockBackend {
    async fn poll(&self) -> Result<Vec<CallEvent>> {
        // Polling consumes the queue, like a real modem event stream.
        Ok(std::mem::take(&mut *self.lock(&self.0.calls)?))
    }
}

#[async_trait]
impl Contacts for MockBackend {
    async fn list(&self) -> Result<Vec<(String, i64)>> {
        Ok(self
            .lock(&self.0.contacts)?
            .iter()
            .map(|record| (record.uid.clone(), record.timestamp_ms))
            .collect())
    }

    async fn vcards(&self, uids: &[String]) -> Result<Vec<ContactRecord>> {
        Ok(self
            .lock(&self.0.contacts)?
            .iter()
            .filter(|record| uids.contains(&record.uid))
            .cloned()
            .collect())
    }
}

#[async_trait]
impl Display for MockBackend {
    async fn enable(&self, display: VirtualDisplay) -> Result<()> {
        *self.lock(&self.0.display)? = Some(display);
        Ok(())
    }

    async fn disable(&self) -> Result<()> {
        *self.lock(&self.0.display)? = None;
        Ok(())
    }

    async fn is_enabled(&self) -> Result<bool> {
        Ok(self.lock(&self.0.display)?.is_some())
    }
}

#[async_trait]
impl Shell for MockBackend {
    async fn open_url(&self, url: &str) -> Result<()> {
        self.lock(&self.0.shell_urls)?.push(url.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Desktop;

    #[tokio::test]
    async fn clipboard_round_trip() {
        let desktop = MockDesktop::new();
        desktop
            .desktop()
            .clipboard
            .as_ref()
            .unwrap()
            .write_text("hello")
            .await
            .unwrap();
        assert_eq!(desktop.clipboard().as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn notifications_are_recorded() {
        let desktop = MockDesktop::new();
        let notification = Notification::new("App", "Title", "Text");
        desktop
            .desktop()
            .notifier
            .as_ref()
            .unwrap()
            .notify(&notification)
            .await
            .unwrap();
        assert_eq!(desktop.notifications(), vec![notification]);
    }

    #[tokio::test]
    async fn volume_is_clamped() {
        let desktop = MockDesktop::new();
        desktop
            .desktop()
            .volume
            .as_ref()
            .unwrap()
            .set(200, true)
            .await
            .unwrap();
        assert_eq!(
            desktop.volume(),
            VolumeState {
                percent: 100,
                muted: true
            }
        );
    }

    #[tokio::test]
    async fn input_is_recorded() {
        let desktop = MockDesktop::new();
        let input = desktop.desktop().input.clone().unwrap();
        input
            .pointer(PointerEvent::movement(1.0, -2.0))
            .await
            .unwrap();
        input
            .key(KeyEvent::Text {
                text: "a".to_string(),
                modifiers: crate::Modifiers::default(),
            })
            .await
            .unwrap();
        assert_eq!(desktop.input_events().len(), 2);
    }

    #[tokio::test]
    async fn screensaver_and_shell_are_recorded() {
        let desktop = MockDesktop::new();
        assert!(!desktop.inhibited());
        desktop
            .desktop()
            .screensaver
            .as_ref()
            .unwrap()
            .inhibit("test")
            .await
            .unwrap();
        assert!(desktop.inhibited());
        desktop
            .desktop()
            .shell
            .as_ref()
            .unwrap()
            .open_url("https://example.com")
            .await
            .unwrap();
        assert_eq!(desktop.shell_urls(), vec!["https://example.com"]);
    }

    #[test]
    fn empty_desktop_has_no_capabilities() {
        assert_eq!(Desktop::default().capabilities(), Default::default());
        assert!(MockDesktop::new().desktop().capabilities().clipboard);
        assert!(MockDesktop::new().desktop().capabilities().lock);
        assert!(!MockDesktop::new().desktop().capabilities().files);
    }

    #[tokio::test]
    async fn lock_calls_are_counted() {
        let desktop = MockDesktop::new();
        assert_eq!(desktop.lock_calls(), 0);
        let lock = desktop.desktop().lock.unwrap();
        lock.lock().await.unwrap();
        lock.lock().await.unwrap();
        assert_eq!(desktop.lock_calls(), 2);
    }

    #[tokio::test]
    async fn files_adapter_saves_and_lists_a_file() {
        use tokio::io::AsyncWriteExt;

        use crate::files::IncomingRequest;

        let desktop = MockDesktop::with_files().unwrap();
        assert!(desktop.desktop().capabilities().files);
        let files = desktop.desktop().files.clone().unwrap();
        let mut incoming = files
            .open_incoming(IncomingRequest {
                filename: Some("photo.jpg".to_string()),
                size: Some(3),
                open: false,
            })
            .await
            .unwrap();
        incoming.writer().write_all(b"abc").await.unwrap();
        let path = incoming.commit().await.unwrap();

        assert_eq!(path, desktop.incoming_dir().unwrap().join("photo.jpg"));
        assert_eq!(desktop.saved_files(), vec![path]);
    }
}
