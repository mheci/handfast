//! Pure environment-based session detection.
//!
//! `detect_session` inspects only process environment variables, so it works
//! identically on Linux (the target), Windows CI hosts and macOS dev boxes.
//! It never touches the compositor and never fails: unknown environments
//! simply yield [`SessionKind::Unknown`].

/// The kind of graphical session the daemon appears to run in.
///
/// Detection is best-effort and purely env-driven; see [`detect_session`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A Wayland compositor session (`WAYLAND_DISPLAY` / `WAYLAND_SOCKET`,
    /// or `XDG_SESSION_TYPE=wayland`). This is Handfast's primary target.
    Wayland,
    /// An X11 session (`XDG_SESSION_TYPE=x11`, or `DISPLAY` set without any
    /// Wayland indicator). Handfast deliberately ships no X11 backends;
    /// this variant exists so callers can report a clear reason instead.
    X11,
    /// A plain text console (`XDG_SESSION_TYPE=tty`).
    Tty,
    /// No interactive display at all (e.g. system service without a seat,
    /// `XDG_SESSION_TYPE=headless`, or a CI runner).
    Headless,
    /// Nothing conclusive could be determined from the environment.
    Unknown,
}

/// Best-effort description of the detected session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// The detected session kind.
    pub kind: SessionKind,
    /// Best-effort compositor guess: `"GNOME"`, `"KDE Plasma"`, `"Sway"`,
    /// `"Hyprland"` or `"wlroots"`. `None` when no signal is available.
    pub compositor: Option<&'static str>,
    /// Wayland protocol globals the *detected* compositor is known to
    /// advertise. This is a static heuristic table (no registry roundtrip has
    /// happened yet); treat it as advisory until `input::VirtualInput::connect`
    /// verifies against the live registry.
    ///
    /// Well-known names used here:
    /// - `zwp_virtual_keyboard_manager_v1`
    /// - `zwlr_virtual_pointer_manager_v1`
    /// - `zwp_idle_inhibit_manager_v1`
    /// - `zwlr_data_control_device_manager_v1`
    /// - `ext_data_control_device_manager_v1`
    pub protocols: Vec<&'static str>,
}

fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Guess the compositor from environment signals.
///
/// Precedence: direct socket variables (`SWAYSOCK`,
/// `HYPRLAND_INSTANCE_SIGNATURE`) beat desktop hints
/// (`XDG_CURRENT_DESKTOP`, `DESKTOP_SESSION`), which beat wlroots socket
/// naming conventions (`WAYLAND_DISPLAY=wlroots-*`).
fn detect_compositor() -> Option<&'static str> {
    if non_empty("SWAYSOCK").is_some() {
        return Some("Sway");
    }
    if non_empty("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Some("Hyprland");
    }
    if let Ok(desktops) = std::env::var("XDG_CURRENT_DESKTOP") {
        let desktops = desktops.to_ascii_lowercase();
        if desktops.split(':').any(|d| d == "gnome") {
            return Some("GNOME");
        }
        if desktops
            .split(':')
            .any(|d| d == "kde" || d.contains("plasma"))
        {
            return Some("KDE Plasma");
        }
    }
    if let Ok(session) = std::env::var("DESKTOP_SESSION") {
        let session = session.to_ascii_lowercase();
        if session.contains("plasma") || session.contains("kde") {
            return Some("KDE Plasma");
        }
    }
    if let Some(display) = non_empty("WAYLAND_DISPLAY") {
        if display.starts_with("wlroots-") {
            return Some("wlroots");
        }
    }
    None
}

/// Static heuristic of the data-control / input protocol globals a given
/// compositor advertises. Phase-4 validation targets refine this table; see
/// the crate-level documentation for the feature → protocol → compositor map.
fn advertised_protocols(compositor: Option<&'static str>) -> Vec<&'static str> {
    match compositor {
        // wlroots-family compositors implement the full wlr stack.
        Some("Sway") | Some("Hyprland") | Some("wlroots") => vec![
            "zwp_virtual_keyboard_manager_v1",
            "zwlr_virtual_pointer_manager_v1",
            "zwp_idle_inhibit_manager_v1",
            "zwlr_data_control_device_manager_v1",
        ],
        // KWin implements zwp virtual keyboard + idle inhibit and (recent)
        // ext-data-control; its pointer support is upstream zwp_virtual_pointer
        // (not yet bound in Rust), hence portal-ei is preferred there.
        Some("KDE Plasma") => vec![
            "zwp_virtual_keyboard_manager_v1",
            "zwp_idle_inhibit_manager_v1",
            "ext_data_control_device_manager_v1",
        ],
        // Mutter implements zwp virtual keyboard and ext-data-control (48+);
        // it lacks both zwlr pointer and zwp idle-inhibit, so DBus/portal-ei
        // strategies are mandatory there.
        Some("GNOME") => vec![
            "zwp_virtual_keyboard_manager_v1",
            "ext_data_control_device_manager_v1",
        ],
        _ => Vec::new(),
    }
}
/// Purely environment-based session detection (works on all platforms).
///
/// Signals consulted, in order:
/// 1. `WAYLAND_DISPLAY` / `WAYLAND_SOCKET` set ⇒ [`SessionKind::Wayland`].
/// 2. `XDG_SESSION_TYPE`: `wayland` | `x11` | `tty` | `headless` |
///    anything else ⇒ Wayland | X11 | Tty | Headless | Unknown.
/// 3. `DISPLAY` set without any other indicator ⇒ [`SessionKind::X11`].
/// 4. Otherwise [`SessionKind::Unknown`] (compositor stays best-effort).
pub fn detect_session() -> SessionInfo {
    let wayland_socket =
        non_empty("WAYLAND_DISPLAY").is_some() || non_empty("WAYLAND_SOCKET").is_some();
    let session_type = non_empty("XDG_SESSION_TYPE").map(|v| v.to_ascii_lowercase());

    let kind = if wayland_socket || session_type.as_deref() == Some("wayland") {
        SessionKind::Wayland
    } else if session_type.as_deref() == Some("x11") {
        SessionKind::X11
    } else if session_type.as_deref() == Some("tty") {
        SessionKind::Tty
    } else if session_type.as_deref() == Some("headless") {
        SessionKind::Headless
    } else if session_type.is_none() && non_empty("DISPLAY").is_some() {
        SessionKind::X11
    } else {
        SessionKind::Unknown
    };

    let compositor = detect_compositor();
    let protocols = advertised_protocols(compositor);
    SessionInfo {
        kind,
        compositor,
        protocols,
    }
}

#[cfg(test)]
mod tests {
    use super::{detect_session, SessionKind};
    use std::ffi::OsStr;
    use std::sync::{Mutex, MutexGuard};

    /// Serialises env mutation across test threads (`serial_test` is not
    /// available in this workspace). Poisoned locks are recovered from so one
    /// panicking test cannot wedge the whole suite.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        match ENV_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Runs `f` with exactly the given environment overrides applied and all
    /// listed keys otherwise cleared; restores prior values afterwards.
    fn with_env(f: impl FnOnce(), overrides: &[(&str, Option<&OsStr>)]) {
        let _guard = lock_env();
        let keys = [
            "WAYLAND_DISPLAY",
            "WAYLAND_SOCKET",
            "XDG_SESSION_TYPE",
            "DISPLAY",
            "TERM",
            "SWAYSOCK",
            "HYPRLAND_INSTANCE_SIGNATURE",
            "XDG_CURRENT_DESKTOP",
            "DESKTOP_SESSION",
        ];
        let mut saved = Vec::with_capacity(keys.len());
        for key in keys {
            saved.push((key, std::env::var_os(key)));
            std::env::remove_var(key);
        }
        for (key, value) in overrides {
            if let Some(value) = value {
                std::env::set_var(key, value);
            }
        }
        f();
        for (key, previous) in saved {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn wayland_display_and_session_type_yield_wayland() {
        with_env(
            || {
                let info = detect_session();
                assert_eq!(info.kind, SessionKind::Wayland);
            },
            &[
                ("WAYLAND_DISPLAY", Some(OsStr::new("wayland-0"))),
                ("XDG_SESSION_TYPE", Some(OsStr::new("wayland"))),
            ],
        );
    }

    #[test]
    fn xdg_session_type_x11_yields_x11() {
        with_env(
            || {
                let info = detect_session();
                assert_eq!(info.kind, SessionKind::X11);
            },
            &[("XDG_SESSION_TYPE", Some(OsStr::new("x11")))],
        );
    }

    #[test]
    fn empty_environment_yields_unknown() {
        with_env(
            || {
                let info = detect_session();
                assert_eq!(info.kind, SessionKind::Unknown);
                assert!(info.compositor.is_none());
                assert!(info.protocols.is_empty());
            },
            &[],
        );
    }

    #[test]
    fn swaysock_implies_sway() {
        with_env(
            || {
                let info = detect_session();
                assert_eq!(info.compositor, Some("Sway"));
                assert!(info.protocols.contains(&"zwp_virtual_keyboard_manager_v1"));
            },
            &[
                ("WAYLAND_DISPLAY", Some(OsStr::new("wayland-1"))),
                ("SWAYSOCK", Some(OsStr::new("/run/user/1000/sway-ipc.sock"))),
            ],
        );
    }

    #[test]
    fn hyprland_signature_implies_hyprland() {
        with_env(
            || {
                let info = detect_session();
                assert_eq!(info.compositor, Some("Hyprland"));
            },
            &[
                ("WAYLAND_DISPLAY", Some(OsStr::new("wayland-2"))),
                ("HYPRLAND_INSTANCE_SIGNATURE", Some(OsStr::new("sig_123"))),
            ],
        );
    }

    #[test]
    fn gnome_in_current_desktop_is_detected() {
        with_env(
            || {
                let info = detect_session();
                assert_eq!(info.compositor, Some("GNOME"));
            },
            &[
                ("WAYLAND_DISPLAY", Some(OsStr::new("wayland-3"))),
                ("XDG_CURRENT_DESKTOP", Some(OsStr::new("ubuntu:GNOME"))),
            ],
        );
    }
}
