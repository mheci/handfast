//! XDG-aware application directory resolution.
//!
//! [`Paths::init`] resolves the four directories Handfast needs and creates
//! them if they do not yet exist, making the call idempotent.
//!
//! On Unix the layout follows the [XDG Base Directory Specification]:
//!
//! | Directory | Base (first match wins)                                   |
//! |-----------|-----------------------------------------------------------|
//! | config    | `$XDG_CONFIG_HOME`, else `$HOME/.config`                  |
//! | data      | `$XDG_DATA_HOME`, else `$HOME/.local/share`               |
//! | cache     | `$XDG_CACHE_HOME`, else `$HOME/.cache`                    |
//! | runtime   | `$XDG_RUNTIME_DIR`, else `$TMPDIR/handfast-{uid}`         |
//!
//! On Windows: `%APPDATA%\handfast` (config and data),
//! `%LOCALAPPDATA%\handfast` (cache) and `%TEMP%\handfast` (runtime).
//!
//! [XDG Base Directory Specification]: https://specifications.freedesktop.org/basedir-spec/latest/

use std::path::PathBuf;

use crate::error::Result;
use crate::APP_NAME;

/// Resolved application directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Persistent configuration (`$XDG_CONFIG_HOME/handfast`).
    pub config: PathBuf,
    /// Persistent state such as certificates and the SQLite database
    /// (`$XDG_DATA_HOME/handfast`).
    pub data: PathBuf,
    /// Regenerable caches (`$XDG_CACHE_HOME/handfast`).
    pub cache: PathBuf,
    /// Sockets and other files that must not survive a reboot
    /// (`$XDG_RUNTIME_DIR` or a per-uid temp directory fallback).
    pub runtime: PathBuf,
}

impl Paths {
    /// Resolve XDG directories for [`APP_NAME`], creating every directory.
    ///
    /// On Unix the `config` and `data` directories are chmod-ed to `0o700`
    /// after creation. The call is idempotent and safe to repeat.
    pub fn init() -> Result<Paths> {
        let paths = resolve();
        create_all(&paths)?;

        #[cfg(unix)]
        {
            restrict_private(&paths.config)?;
            restrict_private(&paths.data)?;
        }

        Ok(paths)
    }
}

fn create_all(paths: &Paths) -> Result<()> {
    for dir in [&paths.config, &paths.data, &paths.cache, &paths.runtime] {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// Read a non-empty environment variable as a directory path.
fn env_dir(var: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

#[cfg(unix)]
fn resolve() -> Paths {
    // Per-uid fallback keeps parallel test users from colliding in $TMPDIR.
    let uid_suffix = match current_uid() {
        Some(uid) => format!("{APP_NAME}-{uid}"),
        None => APP_NAME.to_string(),
    };
    // A missing $HOME is unusual; fall back to the temp dir rather than
    // failing daemon start-up over a cosmetic path choice.
    let home = env_dir("HOME").unwrap_or_else(std::env::temp_dir);
    let config_base = env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
    let data_base = env_dir("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
    let cache_base = env_dir("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"));
    let runtime_base =
        env_dir("XDG_RUNTIME_DIR").unwrap_or_else(|| std::env::temp_dir().join(uid_suffix));

    Paths {
        config: config_base.join(APP_NAME),
        data: data_base.join(APP_NAME),
        cache: cache_base.join(APP_NAME),
        runtime: runtime_base,
    }
}

#[cfg(not(unix))]
fn resolve() -> Paths {
    let appdata = env_dir("APPDATA")
        .or_else(|| {
            env_dir("USERPROFILE").map(|home| home.join("AppData").join("Roaming"))
        })
        .unwrap_or_else(std::env::temp_dir);
    let local_appdata = env_dir("LOCALAPPDATA")
        .or_else(|| {
            env_dir("USERPROFILE").map(|home| home.join("AppData").join("Local"))
        })
        .unwrap_or_else(std::env::temp_dir);

    Paths {
        config: appdata.join(APP_NAME),
        data: appdata.join(APP_NAME),
        cache: local_appdata.join(APP_NAME),
        runtime: std::env::temp_dir().join(APP_NAME),
    }
}

/// Best-effort effective-uid detection by parsing `/proc/self/status`.
///
/// Returns `None` when `/proc` is unavailable. Field layout of the `Uid:` line
/// is `<real>\t<effective>\t<saved>\t<fs>`; we want the effective uid.
#[cfg(target_os = "linux")]
fn current_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("Uid:") else {
            continue;
        };
        return rest.split_whitespace().nth(1)?.parse::<u32>().ok();
    }
    None
}

/// Non-Linux Unix hosts have no portable safe uid probe; treat as unknown.
#[cfg(all(unix, not(target_os = "linux")))]
fn current_uid() -> Option<u32> {
    None
}

/// Restrict `path` to owner-only access (`0o700`). Unix only.
#[cfg(unix)]
fn restrict_private(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_all_directories_idempotently() {
        let first = Paths::init().expect("init should not fail on any platform");
        assert!(first.config.is_dir(), "config dir missing: {:?}", first.config);
        assert!(first.data.is_dir(), "data dir missing: {:?}", first.data);
        assert!(first.cache.is_dir(), "cache dir missing: {:?}", first.cache);
        assert!(
            first.runtime.is_dir(),
            "runtime dir missing: {:?}",
            first.runtime
        );

        let second = Paths::init().expect("second init must succeed");
        assert_eq!(first, second, "resolution must be deterministic");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for private_dir in [&first.config, &first.data] {
                let mode = std::fs::metadata(private_dir)
                    .expect("metadata")
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o700, "{private_dir:?} is not owner-only");
            }
        }
    }
}
