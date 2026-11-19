//! Virtual displays through `wlay` or `weston`.
//!
//! `kdeconnect.virtualmonitor` asks the desktop to create an extra output the
//! phone can extend onto. On Wayland that is not something a client can do
//! directly: the compositor owns the output list. Handfast drives a headless
//! compositor instead, which is exactly what a virtual monitor is — a second
//! output surface that renders into a buffer and is streamed to the phone.
//!
//! Two programs are supported because the ecosystem has no single answer:
//!
//! - `wlay` (`wlay -o <width>x<height>`) creates a virtual output on wlroots
//!   compositors.
//! - `weston` runs its own headless backend, which works on any compositor.
//!
//! The adapter is honest about its limits. If neither program is installed it
//! reports `Unsupported`, the plugin stays disabled, and the phone never sees
//! a virtual monitor capability it cannot use.

use std::io;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::VirtualDisplay;
use crate::traits::Display;

/// Upper bound on a virtual display dimension.
///
/// A phone can ask for anything; a compositor that tries to allocate
/// 100000x100000 will fail in a way that is much harder to diagnose than a
/// refusal.
pub const MAX_DIMENSION: u32 = 8192;

/// Runs the backend program. Tests substitute a fake.
pub trait Spawner: Send + Sync {
    /// Start `program` with `args`, returning a handle that can be killed.
    ///
    /// # Errors
    /// Fails when the process cannot be started.
    fn spawn(&self, program: &str, args: &[String]) -> io::Result<Arc<dyn Handle>>;
}

/// A started backend process.
pub trait Handle: Send + Sync {
    /// Stop the process. Waiting for it is not required.
    fn kill(&self);
}

/// [`Spawner`] backed by [`std::process::Command`].
#[derive(Debug, Clone, Copy, Default)]
pub struct StdSpawner;

struct StdHandle {
    child: Mutex<Option<Child>>,
}

impl Handle for StdHandle {
    fn kill(&self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

impl Spawner for StdSpawner {
    fn spawn(&self, program: &str, args: &[String]) -> io::Result<Arc<dyn Handle>> {
        let child = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Arc::new(StdHandle {
            child: Mutex::new(Some(child)),
        }))
    }
}

/// Which program to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// `wlay`, a virtual output for wlroots compositors.
    Wlay,
    /// `weston`, a headless compositor.
    Weston,
}

impl Backend {
    /// The program name.
    #[must_use]
    pub const fn program(self) -> &'static str {
        match self {
            Self::Wlay => "wlay",
            Self::Weston => "weston",
        }
    }

    /// The command line for a display geometry.
    #[must_use]
    pub fn args(self, display: VirtualDisplay) -> Vec<String> {
        match self {
            Self::Wlay => vec![
                "-o".to_string(),
                format!("{}x{}", display.width, display.height),
            ],
            Self::Weston => vec![
                format!("--width={}", display.width),
                format!("--height={}", display.height),
                "--backend=headless-backend.so".to_string(),
                "--socket=handfast-virtual".to_string(),
            ],
        }
    }
}

/// Validate a requested geometry.
///
/// # Errors
///
/// Fails for a zero or oversized dimension, or a scale outside 50-400%.
pub fn validate(display: VirtualDisplay) -> Result<()> {
    if display.width == 0 || display.height == 0 {
        return Err(DesktopError::Backend(
            "virtual display has a zero dimension".to_string(),
        ));
    }
    if display.width > MAX_DIMENSION || display.height > MAX_DIMENSION {
        return Err(DesktopError::Backend(format!(
            "virtual display exceeds {MAX_DIMENSION} pixels on a side"
        )));
    }
    if !(50..=400).contains(&display.scale) {
        return Err(DesktopError::Backend(
            "virtual display scale is outside 50-400%".to_string(),
        ));
    }
    Ok(())
}

/// Drives a headless compositor as a virtual output.
pub struct HeadlessDisplay {
    backend: Backend,
    spawner: Arc<dyn Spawner>,
    running: Mutex<Option<Arc<dyn Handle>>>,
}

impl HeadlessDisplay {
    /// Build an adapter over a specific backend.
    #[must_use]
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            spawner: Arc::new(StdSpawner),
            running: Mutex::new(None),
        }
    }

    /// Build an adapter over a custom spawner.
    #[must_use]
    pub fn with_spawner(backend: Backend, spawner: Arc<dyn Spawner>) -> Self {
        Self {
            backend,
            spawner,
            running: Mutex::new(None),
        }
    }

    /// The backend this adapter drives.
    #[must_use]
    pub const fn backend(&self) -> Backend {
        self.backend
    }
}

#[async_trait]
impl Display for HeadlessDisplay {
    async fn enable(&self, display: VirtualDisplay) -> Result<()> {
        validate(display)?;
        // Replacing an existing display keeps the adapter idempotent: the
        // phone may re-send the same request after a reconnect.
        if let Some(handle) = self.running.lock().ok().and_then(|mut slot| slot.take()) {
            handle.kill();
        }
        let args = self.backend.args(display);
        let program = self.backend.program();
        let handle = self.spawner.spawn(program, &args).map_err(|error| {
            match error.kind() {
                // `Unsupported` holds a `&'static str`, so the message is
                // spelled out per program rather than formatted.
                io::ErrorKind::NotFound if program == "wlay" => {
                    DesktopError::Unsupported("wlay is not installed")
                }
                io::ErrorKind::NotFound => DesktopError::Unsupported("weston is not installed"),
                _ => DesktopError::Backend(format!("{program}: {error}")),
            }
        })?;
        if let Ok(mut slot) = self.running.lock() {
            *slot = Some(handle);
        }
        Ok(())
    }

    async fn disable(&self) -> Result<()> {
        if let Some(handle) = self.running.lock().ok().and_then(|mut slot| slot.take()) {
            handle.kill();
        }
        Ok(())
    }

    async fn is_enabled(&self) -> Result<bool> {
        Ok(self
            .running
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct CountingHandle {
        kills: AtomicUsize,
    }

    impl Handle for CountingHandle {
        fn kill(&self) {
            self.kills.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct FakeSpawner {
        fail: Option<io::ErrorKind>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
        handles: Mutex<Vec<Arc<CountingHandle>>>,
    }

    impl Spawner for FakeSpawner {
        fn spawn(&self, program: &str, args: &[String]) -> io::Result<Arc<dyn Handle>> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            if let Some(kind) = self.fail {
                return Err(io::Error::new(kind, "fake failure"));
            }
            let handle = Arc::new(CountingHandle::default());
            self.handles.lock().unwrap().push(Arc::clone(&handle));
            Ok(handle as Arc<dyn Handle>)
        }
    }

    fn adapter(
        backend: Backend,
        fail: Option<io::ErrorKind>,
    ) -> (HeadlessDisplay, Arc<FakeSpawner>) {
        let spawner = Arc::new(FakeSpawner {
            fail,
            calls: Mutex::new(Vec::new()),
            handles: Mutex::new(Vec::new()),
        });
        let display =
            HeadlessDisplay::with_spawner(backend, Arc::clone(&spawner) as Arc<dyn Spawner>);
        (display, spawner)
    }

    fn geometry() -> VirtualDisplay {
        VirtualDisplay {
            width: 1280,
            height: 720,
            scale: 100,
        }
    }

    #[test]
    fn wlay_gets_a_geometry_flag() {
        assert_eq!(
            Backend::Wlay.args(geometry()),
            vec!["-o".to_string(), "1280x720".to_string()]
        );
    }

    #[test]
    fn weston_gets_a_headless_backend() {
        let args = Backend::Weston.args(geometry());
        assert!(args.iter().any(|arg| arg.contains("headless")));
        assert!(args.contains(&"--width=1280".to_string()));
        assert!(args.contains(&"--height=720".to_string()));
    }

    #[test]
    fn impossible_geometries_are_refused() {
        let mut display = geometry();
        display.width = 0;
        assert!(validate(display).is_err());

        display = geometry();
        display.width = MAX_DIMENSION + 1;
        assert!(validate(display).is_err());

        display = geometry();
        display.scale = 0;
        assert!(validate(display).is_err());

        assert!(validate(geometry()).is_ok());
    }

    #[tokio::test]
    async fn enable_spawns_and_tracks_the_process() {
        let (display, spawner) = adapter(Backend::Wlay, None);
        assert!(!display.is_enabled().await.unwrap());

        display.enable(geometry()).await.unwrap();
        assert!(display.is_enabled().await.unwrap());
        assert_eq!(
            spawner.calls.lock().unwrap().clone(),
            vec![(
                "wlay".to_string(),
                vec!["-o".to_string(), "1280x720".to_string()]
            )]
        );
    }

    #[tokio::test]
    async fn enabling_twice_replaces_the_process() {
        let (display, spawner) = adapter(Backend::Wlay, None);
        display.enable(geometry()).await.unwrap();
        display.enable(geometry()).await.unwrap();

        let calls = spawner.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        let handles = spawner.handles.lock().unwrap().clone();
        // The first process was killed before the second started.
        assert_eq!(handles[0].kills.load(Ordering::SeqCst), 1);
        assert_eq!(handles[1].kills.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn disable_kills_the_process() {
        let (display, spawner) = adapter(Backend::Wlay, None);
        display.enable(geometry()).await.unwrap();
        display.disable().await.unwrap();
        assert!(!display.is_enabled().await.unwrap());
        let handles = spawner.handles.lock().unwrap().clone();
        assert_eq!(handles[0].kills.load(Ordering::SeqCst), 1);

        // Disabling again is harmless.
        display.disable().await.unwrap();
    }

    #[tokio::test]
    async fn a_missing_program_is_unsupported() {
        let (display, _) = adapter(Backend::Weston, Some(io::ErrorKind::NotFound));
        let error = display.enable(geometry()).await.unwrap_err();
        assert!(
            matches!(&error, DesktopError::Unsupported(message) if message.contains("weston")),
            "unexpected error: {error}"
        );
        assert!(!display.is_enabled().await.unwrap());
    }

    #[tokio::test]
    async fn an_invalid_geometry_never_spawns() {
        let (display, spawner) = adapter(Backend::Wlay, None);
        let mut display_geometry = geometry();
        display_geometry.width = 0;
        assert!(display.enable(display_geometry).await.is_err());
        assert!(spawner.calls.lock().unwrap().is_empty());
    }
}
