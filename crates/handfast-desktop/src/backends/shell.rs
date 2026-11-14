//! URL opening through `xdg-open`.

use std::io;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::traits::Shell;

const PROGRAM: &str = "xdg-open";

/// Starts one external process. Tests substitute a fake.
pub trait Spawner: Send + Sync {
    /// Start `program` with `args` and return without waiting for it to exit.
    ///
    /// # Errors
    /// Fails when the process cannot be started.
    fn spawn(&self, program: &str, args: &[&str]) -> io::Result<()>;
}

/// [`Spawner`] backed by [`tokio::process::Command`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioSpawner;

impl Spawner for TokioSpawner {
    fn spawn(&self, program: &str, args: &[&str]) -> io::Result<()> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        // The handler can outlive xdg-open, so reap it in the background
        // instead of blocking the caller.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Ok(())
    }
}

/// URL opening through `xdg-open`.
pub struct XdgOpenShell {
    spawner: Arc<dyn Spawner>,
}

impl XdgOpenShell {
    /// Build a shell adapter that runs the real `xdg-open`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            spawner: Arc::new(TokioSpawner),
        }
    }

    /// Build a shell adapter over a custom spawner.
    #[must_use]
    pub fn with_spawner(spawner: Arc<dyn Spawner>) -> Self {
        Self { spawner }
    }
}

impl Default for XdgOpenShell {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Shell for XdgOpenShell {
    async fn open_url(&self, url: &str) -> Result<()> {
        // The URL arrives from a peer, so it must not reach xdg-open as an option.
        if url.is_empty() || url.starts_with('-') {
            return Err(DesktopError::Backend(
                "refusing to open an empty or option-like URL".to_string(),
            ));
        }
        self.spawner.spawn(PROGRAM, &[url]).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                DesktopError::Unsupported("xdg-open is not installed")
            } else {
                DesktopError::Backend(format!("failed to run {PROGRAM}: {error}"))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct FakeSpawner {
        failure: Option<io::ErrorKind>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeSpawner {
        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
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

    fn shell(failure: Option<io::ErrorKind>) -> (XdgOpenShell, Arc<FakeSpawner>) {
        let spawner = Arc::new(FakeSpawner {
            failure,
            calls: Mutex::new(Vec::new()),
        });
        let shell = XdgOpenShell::with_spawner(Arc::clone(&spawner) as Arc<dyn Spawner>);
        (shell, spawner)
    }

    #[tokio::test]
    async fn open_url_passes_the_url_as_one_argument() {
        let (shell, spawner) = shell(None);

        shell.open_url("https://example.com/a b").await.unwrap();
        assert_eq!(
            spawner.calls(),
            vec![(
                "xdg-open".to_string(),
                vec!["https://example.com/a b".to_string()]
            )]
        );
    }

    #[tokio::test]
    async fn option_like_url_is_refused() {
        let (shell, spawner) = shell(None);

        let error = shell.open_url("--help").await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Backend(message) if message.contains("refusing")),
            "unexpected error: {error}"
        );
        assert!(spawner.calls().is_empty());
    }

    #[tokio::test]
    async fn missing_tool_is_unsupported() {
        let (shell, _) = shell(Some(io::ErrorKind::NotFound));

        let error = shell.open_url("https://example.com").await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Unsupported(message) if message.contains("xdg-open")),
            "unexpected error: {error}"
        );
    }
}
