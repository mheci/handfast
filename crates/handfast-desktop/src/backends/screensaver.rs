//! Screensaver inhibition through `systemd-inhibit`.

use std::io;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::traits::Screensaver;

const PROGRAM: &str = "systemd-inhibit";

/// A running inhibition. Releasing it stops the inhibit process.
#[async_trait]
pub trait Inhibition: Send + Sync {
    /// Stop the inhibition.
    ///
    /// # Errors
    /// Fails when the inhibit process cannot be stopped.
    async fn release(&self) -> io::Result<()>;
}

/// Starts one inhibit process. Tests substitute a fake.
pub trait Inhibitor: Send + Sync {
    /// Start `systemd-inhibit` with `args`.
    ///
    /// # Errors
    /// Fails when the process cannot be started.
    fn inhibit(&self, args: &[&str]) -> io::Result<Box<dyn Inhibition>>;
}

/// [`Inhibitor`] backed by [`tokio::process::Command`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioInhibitor;

struct ChildInhibition {
    stdin: Mutex<Option<tokio::process::ChildStdin>>,
    child: Mutex<Option<tokio::process::Child>>,
}

#[async_trait]
impl Inhibition for ChildInhibition {
    async fn release(&self) -> io::Result<()> {
        drop(self.stdin.lock().ok().and_then(|mut slot| slot.take()));
        let Some(mut child) = self.child.lock().ok().and_then(|mut slot| slot.take()) else {
            return Ok(());
        };
        if child.try_wait()?.is_none() {
            child.kill().await?;
        }
        Ok(())
    }
}

impl Inhibitor for TokioInhibitor {
    fn inhibit(&self, args: &[&str]) -> io::Result<Box<dyn Inhibition>> {
        let mut child = tokio::process::Command::new(PROGRAM)
            .args(args)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take();
        Ok(Box::new(ChildInhibition {
            stdin: Mutex::new(stdin),
            child: Mutex::new(Some(child)),
        }))
    }
}

/// The one inhibit process, shared by every connected device.
struct Active {
    holders: usize,
    inhibition: Box<dyn Inhibition>,
}

/// Screensaver inhibition through `systemd-inhibit`.
pub struct SystemdInhibitor {
    inhibitor: Arc<dyn Inhibitor>,
    active: tokio::sync::Mutex<Option<Active>>,
}

impl SystemdInhibitor {
    /// Build an inhibitor that runs the real `systemd-inhibit`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inhibitor: Arc::new(TokioInhibitor),
            active: tokio::sync::Mutex::new(None),
        }
    }

    /// Build an inhibitor over a custom spawner.
    #[must_use]
    pub fn with_inhibitor(inhibitor: Arc<dyn Inhibitor>) -> Self {
        Self {
            inhibitor,
            active: tokio::sync::Mutex::new(None),
        }
    }
}

impl Default for SystemdInhibitor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Screensaver for SystemdInhibitor {
    async fn inhibit(&self, reason: &str) -> Result<()> {
        let mut state = self.active.lock().await;
        if let Some(active) = state.as_mut() {
            active.holders += 1;
            return Ok(());
        }
        let why = format!("--why={reason}");
        // `cat` exits when the daemon's stdin pipe closes, so the lock is released even on SIGKILL.
        let args = ["--what=idle", "--who=handfast", why.as_str(), "cat"];
        let inhibition = self.inhibitor.inhibit(&args).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                DesktopError::Unsupported("systemd-inhibit is not installed")
            } else {
                DesktopError::Backend(format!("failed to run {PROGRAM}: {error}"))
            }
        })?;
        *state = Some(Active {
            holders: 1,
            inhibition,
        });
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
        active
            .inhibition
            .release()
            .await
            .map_err(|error| DesktopError::Backend(format!("failed to stop {PROGRAM}: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Clone, Default)]
    struct FakeInhibition {
        releases: Arc<AtomicUsize>,
    }

    impl FakeInhibition {
        fn releases(&self) -> usize {
            self.releases.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Inhibition for FakeInhibition {
        async fn release(&self) -> io::Result<()> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FakeInhibitor {
        failure: Mutex<Option<io::ErrorKind>>,
        calls: Mutex<Vec<Vec<String>>>,
        inhibition: FakeInhibition,
    }

    impl FakeInhibitor {
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Inhibitor for FakeInhibitor {
        fn inhibit(&self, args: &[&str]) -> io::Result<Box<dyn Inhibition>> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| arg.to_string()).collect());
            match self.failure.lock().unwrap().take() {
                Some(kind) => Err(io::Error::from(kind)),
                None => Ok(Box::new(self.inhibition.clone())),
            }
        }
    }

    fn screensaver(failure: Option<io::ErrorKind>) -> (SystemdInhibitor, Arc<FakeInhibitor>) {
        let inhibition = FakeInhibition::default();
        let inhibitor = Arc::new(FakeInhibitor {
            failure: Mutex::new(failure),
            calls: Mutex::new(Vec::new()),
            inhibition: inhibition.clone(),
        });
        let screensaver =
            SystemdInhibitor::with_inhibitor(Arc::clone(&inhibitor) as Arc<dyn Inhibitor>);
        (screensaver, inhibitor)
    }

    fn call(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[tokio::test]
    async fn inhibit_spawns_once_with_the_reason() {
        let (screensaver, inhibitor) = screensaver(None);

        screensaver.inhibit("handfast is connected").await.unwrap();

        assert_eq!(
            inhibitor.calls(),
            vec![call(&[
                "--what=idle",
                "--who=handfast",
                "--why=handfast is connected",
                "cat",
            ])]
        );
        assert_eq!(inhibitor.inhibition.releases(), 0);
    }

    #[tokio::test]
    async fn second_inhibit_is_a_noop() {
        let (screensaver, inhibitor) = screensaver(None);

        screensaver.inhibit("reason").await.unwrap();
        screensaver.inhibit("another reason").await.unwrap();

        assert_eq!(inhibitor.calls().len(), 1);
    }

    #[cfg(unix)]
    fn cat_inhibition() -> ChildInhibition {
        let mut child = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        ChildInhibition {
            stdin: Mutex::new(stdin),
            child: Mutex::new(Some(child)),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_is_held_while_inhibited_and_dropped_on_release() {
        let inhibition = cat_inhibition();
        assert!(inhibition.stdin.lock().unwrap().is_some());

        inhibition.release().await.unwrap();

        assert!(inhibition.stdin.lock().unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closing_stdin_ends_the_inhibited_command() {
        let inhibition = cat_inhibition();
        drop(inhibition.stdin.lock().unwrap().take());

        let mut child = inhibition.child.lock().unwrap().take().unwrap();
        assert!(child.wait().await.unwrap().success());
    }

    #[tokio::test]
    async fn release_kills_the_child() {
        let (screensaver, inhibitor) = screensaver(None);

        screensaver.inhibit("reason").await.unwrap();
        screensaver.release().await.unwrap();

        assert_eq!(inhibitor.inhibition.releases(), 1);
        screensaver.release().await.unwrap();
        assert_eq!(inhibitor.inhibition.releases(), 1);
        assert_eq!(inhibitor.calls().len(), 1);
    }

    #[tokio::test]
    async fn inhibition_is_refcounted_across_holders() {
        let (screensaver, inhibitor) = screensaver(None);

        screensaver.release().await.unwrap();
        assert_eq!(inhibitor.inhibition.releases(), 0);

        screensaver.inhibit("first").await.unwrap();
        screensaver.inhibit("second").await.unwrap();
        assert_eq!(inhibitor.calls().len(), 1);

        screensaver.release().await.unwrap();
        assert_eq!(inhibitor.inhibition.releases(), 0);

        screensaver.release().await.unwrap();
        assert_eq!(inhibitor.inhibition.releases(), 1);

        screensaver.release().await.unwrap();
        assert_eq!(inhibitor.inhibition.releases(), 1);
        assert_eq!(inhibitor.calls().len(), 1);
    }

    #[tokio::test]
    async fn failed_spawn_is_retried_by_the_next_inhibit() {
        let (screensaver, inhibitor) = screensaver(Some(io::ErrorKind::PermissionDenied));

        assert!(screensaver.inhibit("reason").await.is_err());
        screensaver.inhibit("reason").await.unwrap();

        assert_eq!(inhibitor.calls().len(), 2);

        screensaver.release().await.unwrap();
        assert_eq!(inhibitor.inhibition.releases(), 1);
    }

    #[tokio::test]
    async fn missing_tool_is_unsupported() {
        let (screensaver, _) = screensaver(Some(io::ErrorKind::NotFound));

        let error = screensaver.inhibit("reason").await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Unsupported(message) if message.contains("systemd-inhibit")),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn spawn_failure_is_backend_error() {
        let (screensaver, _) = screensaver(Some(io::ErrorKind::PermissionDenied));

        let error = screensaver.inhibit("reason").await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Backend(message) if message.contains("systemd-inhibit")),
            "unexpected error: {error}"
        );
    }
}
