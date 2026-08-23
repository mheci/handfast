//! Idle inhibition (keep the screen awake while a phone streams media).
//!
//! # Strategies and protocols spoken
//!
//! | Priority | Strategy    | Protocol / interface                    | Compiled when                            |
//! |----------|-------------|------------------------------------------|------------------------------------------|
//! | 1        | native      | `zwp_idle_inhibit_unstable_v1` (`zwp_idle_inhibit_manager_v1`) | unix + (`zwp-input` ∨ `zwp-clipboard`) |
//! | 2        | session bus | `org.freedesktop.ScreenSaver.Inhibit`    | unix + `dbus-idle`                       |
//!
//! Ordering is compositor-aware: Mutter (GNOME) implements neither
//! idle-inhibit nor honors inhibitors on role-less surfaces, and KWin counts
//! only *visible* windows, so GNOME/KDE sessions try D-Bus first; the wlroots
//! family (Sway 1.9+, Hyprland) tries the Wayland protocol first. Each
//! strategy falls back to the next on failure; total failure yields
//! [`Error::Unsupported`] — never an abort. All compositors listed are
//! Phase-4 validation targets.
//!
//! Naming note: upstream calls this protocol `zwp_idle_inhibit_unstable_v1`
//! (a freedesktop `wp` protocol), *not* `zwlr_*`; GNOME does not implement it.
use crate::error::{Error, Result};

/// A held idle inhibitor; dropping it releases the inhibitor.
pub struct IdleInhibit {
    /// RAII guard; the inner inhibitors release themselves on drop.
    _handle: Handle,
}

/// Acquired strategy marker. Variants exist purely as RAII guards whose
/// inner types self-release on drop, so their payloads are never read.
#[allow(dead_code)]
enum Handle {
    /// Native Wayland inhibitor over `zwp_idle_inhibit_unstable_v1`.
    #[cfg(all(unix, any(feature = "zwp-input", feature = "zwp-clipboard")))]
    Wayland(Box<wayland::WaylandInhibitor>),
    /// `org.freedesktop.ScreenSaver` cookie on the session bus.
    #[cfg(all(unix, feature = "dbus-idle"))]
    Dbus(dbus_imp::DbusInhibitor),
}

impl IdleInhibit {
    /// Acquire an inhibitor for as long as the returned value lives.
    ///
    /// `reason` is forwarded to the D-Bus strategy (the Wayland protocol has
    /// no reason field). Exactly one strategy holds on success; failures of
    /// earlier strategies are logged at `debug`.
    pub fn acquire(reason: &str) -> Result<Self> {
        let prefer_dbus = matches!(
            crate::detect_session().compositor,
            Some("GNOME") | Some("KDE Plasma")
        );

        let (first_name, first_result, second_name, second_result) = if prefer_dbus {
            (
                "org.freedesktop.ScreenSaver",
                acquire_dbus(reason),
                "zwp_idle_inhibit_manager_v1",
                acquire_wayland(),
            )
        } else {
            (
                "zwp_idle_inhibit_manager_v1",
                acquire_wayland(),
                "org.freedesktop.ScreenSaver",
                acquire_dbus(reason),
            )
        };

        match first_result {
            Ok(handle) => return Ok(Self { _handle: handle }),
            Err(e) => {
                tracing::debug!(strategy = first_name, error = %e, "idle-inhibit strategy failed")
            }
        }
        match second_result {
            Ok(handle) => return Ok(Self { _handle: handle }),
            Err(e) => {
                tracing::debug!(strategy = second_name, error = %e, "idle-inhibit fallback failed")
            }
        }

        Err(Error::Unsupported(format!(
            "no idle inhibition mechanism available ({first_name} and {second_name} both failed); see debug logs"
        )))
    }
}

/// Strategy 1 entry point (compiled out without a Wayland stack).
#[cfg(all(unix, any(feature = "zwp-input", feature = "zwp-clipboard")))]
fn acquire_wayland() -> Result<Handle> {
    Ok(Handle::Wayland(Box::new(
        wayland::WaylandInhibitor::acquire()?,
    )))
}

#[cfg(not(all(unix, any(feature = "zwp-input", feature = "zwp-clipboard"))))]
fn acquire_wayland() -> Result<Handle> {
    Err(Error::Unsupported(
        "wayland idle-inhibit requires a wayland-client-backed feature".to_string(),
    ))
}

/// Strategy 2 entry point (compiled out without `dbus-idle`).
#[cfg(all(unix, feature = "dbus-idle"))]
fn acquire_dbus(reason: &str) -> Result<Handle> {
    Ok(Handle::Dbus(dbus_imp::DbusInhibitor::acquire(
        env!("CARGO_PKG_NAME"),
        reason,
    )?))
}

#[cfg(not(all(unix, feature = "dbus-idle")))]
fn acquire_dbus(_reason: &str) -> Result<Handle> {
    Err(Error::Unsupported(
        "ScreenSaver inhibition requires the dbus-idle feature".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::IdleInhibit;
    use crate::error::{Error, Result};

    /// Without any display (and without a session bus) acquisition must fail
    /// gracefully with a typed error — on every OS and feature combination.
    #[test]
    fn acquire_headless_degrades_gracefully() -> Result<()> {
        let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("WAYLAND_SOCKET").is_some();
        let has_bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty());
        if has_wayland || has_bus {
            return Ok(()); // real-session CI: nothing to assert here
        }
        match IdleInhibit::acquire("handfast-test") {
            Err(_) => Ok(()), // any typed error is acceptable headless
            Ok(_) => Err(Error::Other(
                "inhibitor acquired without any session".to_string(),
            )),
        }
    }
}

#[cfg(all(unix, any(feature = "zwp-input", feature = "zwp-clipboard")))]
mod wayland {
    //! Native `zwp_idle_inhibit_unstable_v1` inhibitor over its own tiny
    //! Wayland connection, backed by a hidden 1×1 mapped surface.
    //!
    //! wlroots-family compositors count inhibitors whose surface is mapped;
    //! a single transparent committed pixel achieves mapping without any
    //! windowing role. KWin/Mutter behavior differs (see module docs) which
    //! is why the D-Bus strategy exists.
    use crate::error::{Error, Result};
    use std::io::Write as _;
    use std::os::fd::AsFd as _;

    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    };
    use wayland_client::{Connection, Dispatch, Proxy as _, QueueHandle};
    use wayland_protocols::wp::idle_inhibit::zv1::client::{
        zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
        zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
    };

    /// `wl_compositor` v4 adds `damage_buffer`; newer versions are unused.
    const COMPOSITOR_MAX_VERSION: u32 = 4;
    /// One ARGB8888 pixel.
    const PIXEL_BYTES: i32 = 4;

    /// Globals discovered during the registry scan.
    #[derive(Default)]
    struct Setup {
        compositor: Option<wl_compositor::WlCompositor>,
        shm: Option<wl_shm::WlShm>,
        manager: Option<ZwpIdleInhibitManagerV1>,
    }

    /// Held resources of the native inhibitor.
    pub(super) struct WaylandInhibitor {
        conn: Connection,
        /// Backing storage of the shm pool (kept until teardown).
        _pixel_file: std::fs::File,
        surface: wl_surface::WlSurface,
        buffer: wl_buffer::WlBuffer,
        pool: wl_shm_pool::WlShmPool,
        inhibitor: ZwpIdleInhibitorV1,
    }

    impl WaylandInhibitor {
        /// Connect, map a hidden surface, and attach an inhibitor to it.
        pub(super) fn acquire() -> Result<Self> {
            let has_env = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
                .iter()
                .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
            if !has_env {
                return Err(Error::Unsupported(
                    "native idle inhibition requires a Wayland session".to_string(),
                ));
            }

            let conn = Connection::connect_to_env()
                .map_err(|e| Error::Other(format!("wayland connect: {e}")))?;
            let mut queue = conn.new_event_queue::<Setup>();
            let qh = queue.handle();
            let mut setup = Setup::default();
            conn.display().get_registry(&qh, ());
            queue
                .roundtrip(&mut setup)
                .map_err(|e| Error::Other(format!("wayland registry roundtrip: {e}")))?;
            queue
                .roundtrip(&mut setup)
                .map_err(|e| Error::Other(format!("wayland bind roundtrip: {e}")))?;

            let mut missing: Vec<&'static str> = Vec::new();
            if setup.compositor.is_none() {
                missing.push("wl_compositor");
            }
            if setup.shm.is_none() {
                missing.push("wl_shm");
            }
            if setup.manager.is_none() {
                missing.push("zwp_idle_inhibit_manager_v1");
            }
            if !missing.is_empty() {
                return Err(Error::ProtocolMissing(missing.join(", ")));
            }
            let compositor = setup.compositor.clone().ok_or_else(|| {
                Error::Other("internal: compositor vanished after scan".to_string())
            })?;
            let shm = setup
                .shm
                .clone()
                .ok_or_else(|| Error::Other("internal: shm vanished after scan".to_string()))?;
            let manager = setup
                .manager
                .clone()
                .ok_or_else(|| Error::Other("internal: manager vanished after scan".to_string()))?;

            // Anonymous temp file stands in for memfd (workspace forbids unsafe);
            // the compositor reads the pixels server-side from this fd.
            let mut pixel_file = tempfile::tempfile()?;
            pixel_file.write_all(&[0, 0, 0, 0])?;
            pixel_file.flush()?;

            let pool: wl_shm_pool::WlShmPool =
                shm.create_pool(pixel_file.as_fd(), PIXEL_BYTES, &qh, ());
            let buffer: wl_buffer::WlBuffer =
                pool.create_buffer(0, 1, 1, PIXEL_BYTES, wl_shm::Format::Argb8888, &qh, ());
            let surface: wl_surface::WlSurface = compositor.create_surface(&qh, ());
            surface.attach(Some(&buffer), 0, 0);
            if surface.version() >= 4 {
                surface.damage_buffer(0, 0, 1, 1);
            } else {
                surface.damage(0, 0, 1, 1);
            }
            surface.commit();

            let inhibitor = manager.create_inhibitor(&surface, &qh, ());
            conn.flush()
                .map_err(|e| Error::Other(format!("wayland flush: {e}")))?;
            tracing::debug!("acquired zwp_idle_inhibitor_v1 on hidden 1x1 surface");
            Ok(Self {
                conn,
                _pixel_file: pixel_file,
                surface,
                buffer,
                pool,
                inhibitor,
            })
        }
    }

    impl Drop for WaylandInhibitor {
        fn drop(&mut self) {
            self.inhibitor.destroy();
            self.surface.destroy();
            self.buffer.destroy();
            self.pool.destroy();
            if let Err(e) = self.conn.flush() {
                tracing::debug!(error = %e, "idle inhibitor cleanup flush failed");
            }
        }
    }

    /// Compositor events carry no data we act on; binding exists solely to
    /// create the inhibitor's 1x1 shm surface.
    impl Dispatch<wl_compositor::WlCompositor, ()> for Setup {
        fn event(
            _state: &mut Self,
            _proxy: &wl_compositor::WlCompositor,
            _event: <wl_compositor::WlCompositor as wayland_client::Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for Setup {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            else {
                return;
            };
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor =
                        Some(registry.bind(name, version.min(COMPOSITOR_MAX_VERSION), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zwp_idle_inhibit_manager_v1" => {
                    let manager: ZwpIdleInhibitManagerV1 =
                        registry.bind(name, version.min(1), qh, ());
                    state.manager = Some(manager);
                }
                other => {
                    tracing::trace!(interface = other, "ignoring wayland global");
                }
            }
        }
    }

    impl Dispatch<wl_surface::WlSurface, ()> for Setup {
        fn event(
            _: &mut Self,
            _: &wl_surface::WlSurface,
            _: wl_surface::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wl_buffer::WlBuffer, ()> for Setup {
        fn event(
            _: &mut Self,
            _: &wl_buffer::WlBuffer,
            _: wl_buffer::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wl_shm::WlShm, ()> for Setup {
        fn event(
            _: &mut Self,
            _: &wl_shm::WlShm,
            _: wl_shm::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wl_shm_pool::WlShmPool, ()> for Setup {
        fn event(
            _: &mut Self,
            _: &wl_shm_pool::WlShmPool,
            _: wl_shm_pool::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwpIdleInhibitManagerV1, ()> for Setup {
        fn event(
            _: &mut Self,
            _: &ZwpIdleInhibitManagerV1,
            _: <ZwpIdleInhibitManagerV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwpIdleInhibitorV1, ()> for Setup {
        fn event(
            _: &mut Self,
            _: &ZwpIdleInhibitorV1,
            _: <ZwpIdleInhibitorV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
}

#[cfg(all(unix, feature = "dbus-idle"))]
mod dbus_imp {
    //! `org.freedesktop.ScreenSaver` inhibitor over the session bus.
    use crate::error::{Error, Result};

    /// ScreenSaver service interface (GNOME session manager / Plasma PowerDevil).
    #[zbus::proxy(
        interface = "org.freedesktop.ScreenSaver",
        default_service = "org.freedesktop.ScreenSaver",
        default_path = "/org/freedesktop/ScreenSaver"
    )]
    pub(super) trait ScreenSaver {
        /// Acquire an inhibitor cookie; idle is blocked until UnInhibit.
        fn inhibit(&self, application_name: &str, reason_for_inhibit: &str) -> zbus::Result<u32>;
        /// Release a cookie acquired through [`Self::inhibit`].
        fn un_inhibit(&self, cookie: u32) -> zbus::Result<()>;
    }

    /// Held D-Bus inhibitor cookie.
    pub(super) struct DbusInhibitor {
        conn: zbus::blocking::Connection,
        cookie: u32,
    }

    impl DbusInhibitor {
        pub(super) fn acquire(application: &str, reason: &str) -> Result<Self> {
            let conn = zbus::blocking::Connection::session()
                .map_err(|e| Error::Other(format!("session bus unavailable: {e}")))?;
            let proxy = ScreenSaverProxyBlocking::new(&conn)
                .map_err(|e| Error::Other(format!("ScreenSaver proxy: {e}")))?;
            let cookie = proxy
                .inhibit(application, reason)
                .map_err(|e| Error::Other(format!("ScreenSaver.Inhibit: {e}")))?;
            tracing::debug!(cookie, "acquired org.freedesktop.ScreenSaver inhibitor");
            Ok(Self { conn, cookie })
        }
    }

    impl Drop for DbusInhibitor {
        fn drop(&mut self) {
            match ScreenSaverProxyBlocking::new(&self.conn) {
                Ok(proxy) => {
                    if let Err(e) = proxy.un_inhibit(self.cookie) {
                        tracing::warn!(
                            cookie = self.cookie,
                            error = %e,
                            "failed to release ScreenSaver inhibitor"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(cookie = self.cookie, error = %e, "ScreenSaver proxy lost on drop")
                }
            }
        }
    }
}
