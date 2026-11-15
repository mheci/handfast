//! Desktop backends.
//!
//! Each backend is independent, so a host missing one capability still gets the
//! others. Linux adapters run session tools; Windows adapters run in-box
//! executables by absolute path or wrap a safe crate.

pub mod clipboard;
pub mod lock;
pub mod mpris;
pub mod notifications;
pub mod power;
pub mod screensaver;
pub mod shell;
pub mod volume;
#[cfg(windows)]
pub mod win;

use std::path::PathBuf;
use std::sync::Arc;

use crate::traits::{Clipboard, Lock, Media, Notifier, Power, Screensaver, Shell, Volume};

/// Whether `program` resolves to a regular file on `PATH`.
///
/// Nothing is spawned. On Windows the `PATHEXT` suffixes are tried.
#[must_use]
pub fn program_available(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let suffixes = program_suffixes();
    program_in_paths(program, std::env::split_paths(&path), &suffixes)
}

fn program_in_paths(
    program: &str,
    mut paths: impl Iterator<Item = PathBuf>,
    suffixes: &[String],
) -> bool {
    paths.any(|directory| {
        suffixes
            .iter()
            .any(|suffix| directory.join(format!("{program}{suffix}")).is_file())
    })
}

fn program_suffixes() -> Vec<String> {
    if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|suffix| !suffix.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        vec![String::new()]
    }
}

/// Build the host clipboard backend.
#[must_use]
pub fn clipboard() -> Arc<dyn Clipboard> {
    Arc::new(clipboard::CommandClipboard::detect())
}

/// Build the host notification backend.
#[must_use]
pub fn notifier() -> Arc<dyn Notifier> {
    Arc::new(notifications::CommandNotifier::new())
}

/// Build the host MPRIS media backend.
#[must_use]
pub fn media() -> Arc<dyn Media> {
    Arc::new(mpris::PlayerctlMedia::new())
}

/// Build the host system volume backend.
#[must_use]
pub fn volume() -> Arc<dyn Volume> {
    Arc::new(volume::WpctlVolume::new())
}

/// Build the host URL opening backend.
#[must_use]
pub fn shell() -> Arc<dyn Shell> {
    Arc::new(shell::XdgOpenShell::new())
}

/// Build the host screensaver inhibition backend.
#[must_use]
pub fn screensaver() -> Arc<dyn Screensaver> {
    Arc::new(screensaver::SystemdInhibitor::new())
}

/// Build the host session lock backend.
#[must_use]
pub fn lock() -> Arc<dyn Lock> {
    Arc::new(lock::LoginctlLock::new())
}

/// Build the host battery backend.
#[must_use]
pub fn power() -> Arc<dyn Power> {
    Arc::new(power::SysfsPower::new())
}

/// Add every Windows backend that does not need an external install to
/// `desktop`.
///
/// The toast and SMTC adapters need the in-box Windows PowerShell. The others
/// use System32 executables or a safe crate, so they are always present.
#[cfg(windows)]
#[must_use]
pub fn windows(mut desktop: crate::Desktop) -> crate::Desktop {
    desktop.clipboard = Some(Arc::new(win::WinClipboard::new()));
    desktop.screensaver = Some(Arc::new(win::WinScreensaver::new()));
    desktop.lock = Some(Arc::new(win::WinLock::new()));
    desktop.shell = Some(Arc::new(win::ExplorerShell::new()));
    desktop.volume = Some(Arc::new(win::WinVolume::new()));
    desktop.power = Some(Arc::new(win::WinPower::new()));
    if win::powershell().is_file() {
        desktop.notifier = Some(Arc::new(win::WinNotifier::new()));
        desktop.media = Some(Arc::new(win::WinMedia::new()));
    }
    desktop
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn scan(program: &str, dirs: &[&Path], suffixes: &[&str]) -> bool {
        let suffixes: Vec<String> = suffixes
            .iter()
            .map(|suffix| (*suffix).to_string())
            .collect();
        program_in_paths(
            program,
            dirs.iter().copied().map(Path::to_path_buf),
            &suffixes,
        )
    }

    #[test]
    fn finds_a_regular_file_on_the_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("handfast-test-tool"), b"").unwrap();

        assert!(scan("handfast-test-tool", &[dir.path()], &[""]));
        assert!(!scan("handfast-missing-tool", &[dir.path()], &[""]));
    }

    #[test]
    fn ignores_directories_and_keeps_searching() {
        let first = tempfile::tempdir().unwrap();
        std::fs::create_dir(first.path().join("handfast-test-tool")).unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::write(second.path().join("handfast-test-tool"), b"").unwrap();

        assert!(!scan("handfast-test-tool", &[first.path()], &[""]));
        assert!(scan(
            "handfast-test-tool",
            &[first.path(), second.path()],
            &[""]
        ));
    }

    #[test]
    fn tries_each_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("handfast-test-tool.exe"), b"").unwrap();

        assert!(!scan("handfast-test-tool", &[dir.path()], &[""]));
        assert!(scan("handfast-test-tool", &[dir.path()], &[".exe"]));
    }

    #[test]
    fn missing_program_is_not_available() {
        assert!(!program_available("handfast-definitely-missing-program"));
    }
}
