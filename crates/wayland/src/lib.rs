#![forbid(unsafe_code)]
#![deny(missing_docs)]
//! # handfast-wayland
//!
//! Handfast's Wayland bridge: virtual input injection, clipboard access and
//! change watching, and idle inhibition.
//!
//! **Strictly Wayland-first**: there is no X11 / XWayland / XTEST code path
//! anywhere in this crate (CONTRIBUTING.md ground rule 3). Where compositors
//! lack native Wayland protocols, D-Bus services (`org.freedesktop.ScreenSaver`)
//! and XDG portals (`org.freedesktop.portal.RemoteDesktop`) are used instead.
//! Every fallible operation degrades gracefully to [`Error`] — nothing here
//! may abort the daemon.
//!
//! ## Feature → protocol → compositor matrix
//!
//! All rows are Phase-4 validation targets (headless sway CI runs this
//! phase); the matrix documents intent until validated.
//!
//! | Feature                | Protocol spoken                                                        | Compositors expected to work            |
//! |------------------------|--------------------------------------------------------------------------|------------------------------------------|
//! | `zwp-input` (keyboard) | `zwp_virtual_keyboard_manager_v1`                                       | KDE Plasma 6, GNOME 47+, Sway 1.9+, Hyprland |
//! | `zwp-input` (pointer)  | `zwlr_virtual_pointer_manager_v1` (wlroots)                             | Sway 1.9+, Hyprland, wlroots-based       |
//! | `dbus-idle`            | `org.freedesktop.ScreenSaver.Inhibit` (session bus)                     | KDE Plasma 6, GNOME 47+                  |
//! | *(idle, native)*       | `zwp_idle_inhibit_unstable_v1`                                          | Sway 1.9+, Hyprland, wlroots-based       |
//! | `zwp-clipboard`        | `ext_data_control_v1`, falling back to `wlr_data_control_unstable_v1`   | Plasma 6, GNOME 48+, Sway 1.9+, Hyprland |
//! | `portal-ei`            | `org.freedesktop.portal.RemoteDesktop` + libei/EIS                      | GNOME 47+ (preferred), Plasma 6          |
//!
//! Platform shims: on non-Wayland builds (e.g. Windows CI) every module
//! still compiles and its API reports [`Error::Unsupported`] instead of
//! disappearing, so downstream crates typecheck unchanged.

pub mod clipboard;
pub mod detect;
pub mod error;
pub mod idle;
pub mod input;
#[cfg(feature = "portal-ei")]
pub mod portal_ei;

pub use detect::{detect_session, SessionInfo, SessionKind};
pub use error::{Error, Result};
