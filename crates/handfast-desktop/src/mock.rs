//! In-memory desktop backend for contract tests.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::{KeyEvent, MediaAction, Notification, PlaybackState, PointerEvent, VolumeState};
use crate::traits::{Clipboard, Input, Media, Notifier, Screensaver, Shell, Volume};
use crate::Desktop;

/// Recorded synthetic input.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// Pointer movement or scroll.
    Pointer(PointerEvent),
    /// Keyboard event.
    Key(KeyEvent),
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
}

/// A desktop backend that records every operation.
#[derive(Clone)]
pub struct MockDesktop {
    state: Arc<MockState>,
    desktop: Desktop,
}

impl MockDesktop {
    /// Build a mock desktop with every adapter present.
    #[must_use]
    pub fn new() -> Self {
        let state = Arc::new(MockState::default());
        let backend = Arc::new(MockBackend(Arc::clone(&state)));
        let desktop = Desktop {
            clipboard: Some(Arc::clone(&backend) as Arc<dyn Clipboard>),
            notifier: Some(Arc::clone(&backend) as Arc<dyn Notifier>),
            media: Some(Arc::clone(&backend) as Arc<dyn Media>),
            volume: Some(Arc::clone(&backend) as Arc<dyn Volume>),
            input: Some(Arc::clone(&backend) as Arc<dyn Input>),
            screensaver: Some(Arc::clone(&backend) as Arc<dyn Screensaver>),
            shell: Some(Arc::clone(&backend) as Arc<dyn Shell>),
        };
        Self { state, desktop }
    }

    /// Borrow the composed desktop.
    #[must_use]
    pub fn desktop(&self) -> Desktop {
        self.desktop.clone()
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
}

impl Default for MockDesktop {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
struct MockBackend(Arc<MockState>);

impl MockBackend {
    fn lock<T>(&self, mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
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
            .pointer(PointerEvent {
                dx: 1.0,
                dy: -2.0,
                scroll: false,
            })
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
    }
}
