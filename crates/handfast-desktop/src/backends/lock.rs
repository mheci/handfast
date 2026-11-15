//! Session locking through `loginctl`.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::traits::Lock;

const PROGRAM: &str = "loginctl";

/// Captured result of a finished `loginctl` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockOutput {
    /// Whether the process exited with status zero.
    pub success: bool,
    /// Exit code, or `None` when the process ended without one.
    pub code: Option<i32>,
    /// Bytes written to stderr.
    pub stderr: Vec<u8>,
}

/// Runs one `loginctl` command. Tests substitute a fake.
#[async_trait]
pub trait LockRunner: Send + Sync {
    /// Run `loginctl` with `args`.
    ///
    /// # Errors
    /// Fails when the process cannot be started.
    async fn run(&self, args: &[&str]) -> io::Result<LockOutput>;
}

/// [`LockRunner`] backed by [`tokio::process::Command`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioLockRunner;

#[async_trait]
impl LockRunner for TokioLockRunner {
    async fn run(&self, args: &[&str]) -> io::Result<LockOutput> {
        let output = tokio::process::Command::new(PROGRAM)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(LockOutput {
            success: output.status.success(),
            code: output.status.code(),
            stderr: output.stderr,
        })
    }
}

/// Session locking through `loginctl`.
pub struct LoginctlLock {
    runner: Arc<dyn LockRunner>,
}

impl LoginctlLock {
    /// Build a lock adapter that runs the real `loginctl`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: Arc::new(TokioLockRunner),
        }
    }

    /// Build a lock adapter over a custom runner.
    #[must_use]
    pub fn with_runner(runner: Arc<dyn LockRunner>) -> Self {
        Self { runner }
    }
}

impl Default for LoginctlLock {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Lock for LoginctlLock {
    async fn lock(&self) -> Result<()> {
        let output = self.runner.run(&["lock-session"]).await.map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                DesktopError::Unsupported("loginctl is not installed")
            } else {
                DesktopError::Backend(format!("failed to run {PROGRAM}: {error}"))
            }
        })?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let code = match output.code {
                Some(code) => code.to_string(),
                None => "a signal".to_string(),
            };
            let detail = if stderr.is_empty() {
                format!("{PROGRAM} exited with {code}")
            } else {
                format!("{PROGRAM} exited with {code}: {stderr}")
            };
            return Err(DesktopError::Backend(detail));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct FakeRunner {
        result: Mutex<Option<io::Result<LockOutput>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn returning(result: io::Result<LockOutput>) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Some(result)),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LockRunner for FakeRunner {
        async fn run(&self, args: &[&str]) -> io::Result<LockOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| arg.to_string()).collect());
            self.result
                .lock()
                .unwrap()
                .take()
                .expect("the fake runner is called once")
        }
    }

    fn lock(result: io::Result<LockOutput>) -> (LoginctlLock, Arc<FakeRunner>) {
        let runner = FakeRunner::returning(result);
        let lock = LoginctlLock::with_runner(Arc::clone(&runner) as Arc<dyn LockRunner>);
        (lock, runner)
    }

    #[tokio::test]
    async fn lock_session_locks_the_current_session() {
        let (lock, runner) = lock(Ok(LockOutput {
            success: true,
            code: Some(0),
            stderr: Vec::new(),
        }));

        lock.lock().await.unwrap();
        assert_eq!(runner.calls(), vec![vec!["lock-session".to_string()]]);
    }

    #[tokio::test]
    async fn missing_tool_is_unsupported() {
        let (lock, _) = lock(Err(io::Error::from(io::ErrorKind::NotFound)));

        let error = lock.lock().await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Unsupported(message) if message.contains("loginctl")),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn failed_command_reports_stderr() {
        let (lock, _) = lock(Ok(LockOutput {
            success: false,
            code: Some(1),
            stderr: b"Failed to lock session: Access denied\n".to_vec(),
        }));

        let error = lock.lock().await.unwrap_err();
        assert!(
            matches!(
                &error,
                DesktopError::Backend(message)
                    if message.contains("loginctl exited with 1")
                        && message.contains("Access denied")
            ),
            "unexpected error: {error}"
        );
    }
}
