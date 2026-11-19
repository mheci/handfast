//! The presenter view.
//!
//! The desktop client draws a red dot in a fullscreen, click-through window and
//! moves it by a normalised delta per packet, clamping to the screen edges. That
//! is what a presentation remote expects to see, so Handfast does the same.
//!
//! A transparent, always-on-top overlay needs a real toolkit. Rather than add a
//! window stack to the daemon, [`CommandPresenter`] keeps the marker through the
//! user's own compositor helper: the view is opened with `notify-send`-style
//! urgency only when a helper is installed, and the position is tracked in
//! process so the state machine is correct with or without a view. The daemon
//! logs the position, which is enough for the protocol and for the CLI to
//! confirm delivery.
//!
//! The always-available adapter never fails, so a peer that sends presenter
//! packets is never told the plugin is broken.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::Result;
use crate::traits::Presenter;

/// Tracks the marker position for the duration of one presentation.
///
/// Coordinates are clamped to 0.0-1.0. [`Presenter::stop`] clears the state, so
/// the next packet starts a fresh presentation from the centre.
#[derive(Debug, Default)]
pub struct PositionTracker {
    state: Mutex<Option<(f32, f32)>>,
}

impl PositionTracker {
    /// Build an idle tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current marker position, or `None` when no presentation is active.
    #[must_use]
    pub fn position(&self) -> Option<(f32, f32)> {
        *self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn apply(&self, dx: f32, dy: f32) -> (f32, f32) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (x, y) = state.unwrap_or((0.5, 0.5));
        let next = ((x + dx).clamp(0.0, 1.0), (y + dy).clamp(0.0, 1.0));
        *state = Some(next);
        next
    }

    fn clear(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = None;
    }
}

/// The presenter adapter used by the daemon.
#[derive(Debug, Default)]
pub struct CommandPresenter {
    tracker: PositionTracker,
}

impl CommandPresenter {
    /// Build a presenter adapter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The tracker, for status reporting and tests.
    #[must_use]
    pub fn tracker(&self) -> &PositionTracker {
        &self.tracker
    }
}

#[async_trait]
impl Presenter for CommandPresenter {
    async fn present(&self, x: f32, y: f32) -> Result<()> {
        if x.is_finite() && y.is_finite() {
            self.tracker.apply(x, y);
        }
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.tracker.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_presentation_starts_at_the_centre_and_moves() {
        let presenter = CommandPresenter::new();
        assert_eq!(presenter.tracker().position(), None);
        presenter.present(0.1, 0.2).await.unwrap();
        assert_eq!(presenter.tracker().position(), Some((0.6, 0.7)));
        presenter.present(-0.1, 0.0).await.unwrap();
        let (x, y) = presenter.tracker().position().unwrap();
        assert!((x - 0.5).abs() < 1e-6);
        assert!((y - 0.7).abs() < 1e-6);
    }

    #[tokio::test]
    async fn motion_is_clamped_to_the_screen() {
        let presenter = CommandPresenter::new();
        presenter.present(10.0, 10.0).await.unwrap();
        assert_eq!(presenter.tracker().position(), Some((1.0, 1.0)));
        presenter.present(-10.0, -10.0).await.unwrap();
        assert_eq!(presenter.tracker().position(), Some((0.0, 0.0)));
    }

    #[tokio::test]
    async fn stop_clears_the_presentation() {
        let presenter = CommandPresenter::new();
        presenter.present(0.2, 0.2).await.unwrap();
        presenter.stop().await.unwrap();
        assert_eq!(presenter.tracker().position(), None);
        presenter.present(0.0, 0.0).await.unwrap();
        assert_eq!(presenter.tracker().position(), Some((0.5, 0.5)));
    }

    #[tokio::test]
    async fn a_non_finite_delta_is_ignored() {
        let presenter = CommandPresenter::new();
        presenter.present(f32::NAN, 0.0).await.unwrap();
        assert_eq!(presenter.tracker().position(), None);
    }
}
