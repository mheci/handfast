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
    child: Mutex<Option<tokio::process::Child>>,
}

#[async_trait]
impl Inhibition for ChildInhibition {
    async fn release(&self) -> io::Result<()> {
        let child = self.child.lock().ok().and_then(|mut slot| slot.take());
        if let Some(mut child) = child {
            child.kill().await?;
        }
        Ok(())
    }
}

impl Inhibitor for TokioInhibitor {
    fn inhibit(&self, args: &[&str]) -> io::Result<Box<dyn Inhibition>> {
        let child = tokio::process::Command::new(PROGRAM)
            .args(args)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Box::new(ChildInhibition {
            child: Mutex::new(Some(child)),
        }))
    }
}

/// Screensaver inhibition through `systemd-inhibit`.
pub struct SystemdInhibitor {
    inhibitor: Arc<dyn Inhibitor>,
    active: tokio::sync::Mutex<Option<Box<dyn Inhibition>>>,
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
        let mut active = self.active.lock().await;
        if active.is_some() {
            return Ok(());
        }
        let why = format!("--why={reason}");
        let args = [
            "--what=idle",
            "--who=handfast",
            why.as_str(),
            "sleep",
            "infinity",
        ];
        let inhibition = self.inhibitor.inhibit(&args).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                DesktopError::Unsupported("systemd-inhibit is not installed")
            } else {
                DesktopError::Backend(format!("failed to run {PROGRAM}: {error}"))
            }
        })?;
        *active = Some(inhibition);
        Ok(())
    }

    async fn release(&self) -> Result<()> {
        let mut active = self.active.lock().await;
        if let Some(inhibition) = active.take() {
            inhibition.release().await.map_err(|error| {
                DesktopError::Backend(format!("failed to stop {PROGRAM}: {error}"))
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[derive(Clone, Default)]
    struct FakeInhibition {
        released: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Inhibition for FakeInhibition {
        async fn release(&self) -> io::Result<()> {
            self.released.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FakeInhibitor {
        failure: Option<io::ErrorKind>,
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
            match self.failure {
                Some(kind) => Err(io::Error::from(kind)),
                None => Ok(Box::new(self.inhibition.clone())),
            }
        }
    }

    fn screensaver(failure: Option<io::ErrorKind>) -> (SystemdInhibitor, Arc<FakeInhibitor>) {
        let inhibition = FakeInhibition::default();
        let inhibitor = Arc::new(FakeInhibitor {
            failure,
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
                "sleep",
                "infinity",
            ])]
        );
        assert!(!inhibitor.inhibition.released.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn second_inhibit_is_a_noop() {
        let (screensaver, inhibitor) = screensaver(None);

        screensaver.inhibit("reason").await.unwrap();
        screensaver.inhibit("another reason").await.unwrap();

        assert_eq!(inhibitor.calls().len(), 1);
    }

    #[tokio::test]
    async fn release_kills_the_child() {
        let (screensaver, inhibitor) = screensaver(None);

        screensaver.inhibit("reason").await.unwrap();
        screensaver.release().await.unwrap();

        assert!(inhibitor.inhibition.released.load(Ordering::SeqCst));
        screensaver.release().await.unwrap();
        assert_eq!(inhibitor.calls().len(), 1);
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
