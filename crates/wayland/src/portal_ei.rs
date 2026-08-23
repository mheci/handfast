//! Mediated input via the XDG RemoteDesktop portal (libei-style), the
//! preferred path on compositors that restrict direct virtual-input
//! protocols - notably GNOME.
//!
//! # Why this path exists (the GNOME story)
//!
//! Mutter does not expose the virtual-input Wayland globals used by
//! [`crate::input`] (`zwp_virtual_keyboard_manager_v1`,
//! `zwlr_virtual_pointer_manager_v1`) to ordinary clients; under GNOME those
//! binds fail outright, leaving [`crate::input::VirtualInput`] with
//! [`crate::error::Error::ProtocolMissing`]. The sanctioned injection route
//! on GNOME is
//! `org.freedesktop.portal.RemoteDesktop`: every event is brokered by
//! xdg-desktop-portal and gated by an explicit user-consent dialog, with
//! Mutter speaking libei natively behind it. The daemon should therefore try
//! this backend first on GNOME sessions and fall back to the direct globals
//! on compositors that advertise them (Sway/Hyprland/Plasma).
//!
//! # Protocols spoken
//!
//! | Layer    | Interface                                                              |
//! |----------|-------------------------------------------------------------------------|
//! | portal   | `org.freedesktop.portal.RemoteDesktop` (`CreateSession`, `SelectDevices`, `Start`, `Notify*`) |
//! | transport| portal `Notify*` D-Bus calls (a `ConnectToEIS` libei stream is optional future work) |
//!
//! Compositor matrix: GNOME 47+ (primary target; Mutter prefers libei),
//! KDE Plasma 6 (secondary). Event injection uses the portal's synchronous
//! `Notify*` methods: each call is acknowledged by the portal service before
//! it returns, so events cannot be silently lost and nothing needs to be
//! buffered client-side (see [`PortalInput::flush`]). The higher-throughput
//! `ConnectToEIS` socket remains a possible future optimization that will not
//! change this public API.
//!
//! Unlike `input::VirtualInput` this path never touches compositor globals:
//! everything flows through the portal D-Bus service, subject to user
//! consent. There is no X11/XTEST fallback anywhere. A handle is plain D-Bus
//! proxies underneath, so unlike `input::VirtualInput` it is `Send + Sync`
//! and not tied to a dedicated input thread.

/// Real implementation over ashpd's RemoteDesktop wrapper.
#[cfg(all(unix, feature = "portal-ei"))]
mod imp {
    use crate::error::{Error, Result};

    use ashpd::desktop::remote_desktop::{
        Axis, DeviceType, KeyState, NotifyKeyboardKeysymOptions, NotifyPointerAxisDiscreteOptions,
        NotifyPointerButtonOptions, NotifyPointerMotionOptions, RemoteDesktop,
        SelectDevicesOptions, SelectedDevices,
    };
    use ashpd::desktop::Session;

    /// Portal interface name; used verbatim in [`Error::ProtocolMissing`] so
    /// callers can tell exactly which service was missing.
    const PORTAL: &str = "org.freedesktop.portal.RemoteDesktop";

    /// Handle for a mediated input session backed by a live RemoteDesktop
    /// portal session. Dropping it closes the session best-effort.
    pub struct PortalInput {
        proxy: RemoteDesktop,
        session: Option<Session<RemoteDesktop>>,
        /// Device access actually granted by the user in the Start dialog;
        /// injection methods refuse devices that were denied up front.
        devices: SelectedDevices,
    }

    /// Map an ashpd failure into the crate error type (stage is a static
    /// literal at every call site).
    fn portal_err(stage: &'static str) -> impl Fn(ashpd::Error) -> Error {
        move |e| Error::Other(format!("remote-desktop portal {stage}: {e}"))
    }

    impl PortalInput {
        /// Check whether a RemoteDesktop portal service is reachable.
        ///
        /// Never fails: `Ok(false)` simply means "no portal", letting the
        /// daemon fall back to direct Wayland strategies where available.
        pub async fn probe() -> Result<bool> {
            match RemoteDesktop::new().await {
                Ok(_) => Ok(true),
                Err(e) => {
                    tracing::debug!(error = %e, "remote-desktop portal unreachable");
                    Ok(false)
                }
            }
        }

        /// Run the full portal handshake - `CreateSession`, then
        /// `SelectDevices(keyboard | pointer)`, then `Start` (which surfaces
        /// the desktop's user-consent dialog) - and keep the resulting session
        /// open for event injection until this handle is dropped.
        ///
        /// Errors with [`Error::ProtocolMissing`] when no RemoteDesktop portal
        /// service is reachable on the session bus, and [`Error::Other`] for
        /// handshake failures (user cancellation included). Never falls back
        /// to X11/XTEST.
        pub async fn connect() -> Result<Self> {
            let proxy = RemoteDesktop::new().await.map_err(|e| {
                tracing::debug!(error = %e, "remote-desktop portal unreachable");
                Error::ProtocolMissing(PORTAL.to_string())
            })?;
            let session = proxy
                .create_session(Default::default())
                .await
                .map_err(portal_err("CreateSession"))?;
            proxy
                .select_devices(
                    &session,
                    SelectDevicesOptions::default()
                        .set_devices(DeviceType::Keyboard | DeviceType::Pointer),
                )
                .await
                .map_err(portal_err("SelectDevices"))?;
            let started = proxy
                .start(&session, None, Default::default())
                .await
                .map_err(portal_err("Start"))?;
            let selected = started.response().map_err(portal_err("start response"))?;
            tracing::debug!(
                version = proxy.version(),
                devices = ?selected.devices(),
                "remote-desktop session started"
            );
            Ok(Self {
                proxy,
                session: Some(session),
                devices: selected,
            })
        }

        /// Send a keysym press/release (`NotifyKeyboardKeysym`). `keysym` is
        /// an xkb keysym, matching the convention of
        /// [`crate::input::VirtualInput::key`]; no keymap upload is needed on
        /// this path because the compositor resolves keysyms itself.
        ///
        /// Fails with [`Error::ProtocolMissing`] when keyboard access was not
        /// granted in the consent dialog.
        pub async fn key(&self, keysym: u32, press: bool) -> Result<()> {
            self.require_device(DeviceType::Keyboard, "keyboard")?;
            let keysym = narrow_i32(keysym, "keysym")?;
            self.proxy
                .notify_keyboard_keysym(
                    self.active_session()?,
                    keysym,
                    key_state(press),
                    NotifyKeyboardKeysymOptions::default(),
                )
                .await
                .map_err(portal_err("NotifyKeyboardKeysym"))
        }

        /// Move the pointer by `(dx, dy)` in global compositor space
        /// (`NotifyPointerMotion`).
        ///
        /// Fails with [`Error::ProtocolMissing`] when pointer access was not
        /// granted in the consent dialog.
        pub async fn pointer_move_relative(&self, dx: f64, dy: f64) -> Result<()> {
            self.require_device(DeviceType::Pointer, "pointer")?;
            self.proxy
                .notify_pointer_motion(
                    self.active_session()?,
                    dx,
                    dy,
                    NotifyPointerMotionOptions::default(),
                )
                .await
                .map_err(portal_err("NotifyPointerMotion"))
        }

        /// Press/release a pointer button using linux/input-event-codes values
        /// (`BTN_LEFT = 0x110`, `BTN_RIGHT = 0x111`, ...) via
        /// `NotifyPointerButton`, matching [`crate::input::VirtualInput::button`].
        ///
        /// Fails with [`Error::ProtocolMissing`] when pointer access was not
        /// granted in the consent dialog.
        pub async fn button(&self, button: u32, press: bool) -> Result<()> {
            self.require_device(DeviceType::Pointer, "pointer")?;
            let button = narrow_i32(button, "button code")?;
            self.proxy
                .notify_pointer_button(
                    self.active_session()?,
                    button,
                    key_state(press),
                    NotifyPointerButtonOptions::default(),
                )
                .await
                .map_err(portal_err("NotifyPointerButton"))
        }

        /// Scroll `steps` wheel notches; positive scrolls down/right. Sent as
        /// a discrete axis event (`NotifyPointerAxisDiscrete`) so both smooth-
        /// and discrete-scrolling clients react, mirroring
        /// [`crate::input::VirtualInput::axis`].
        pub async fn axis(&self, vertical: bool, steps: f64) -> Result<()> {
            self.require_device(DeviceType::Pointer, "pointer")?;
            let notches = steps.round();
            if notches == 0.0 {
                return Ok(());
            }
            let discrete = notches.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
            let axis = if vertical {
                Axis::Vertical
            } else {
                Axis::Horizontal
            };
            self.proxy
                .notify_pointer_axis_discrete(
                    self.active_session()?,
                    axis,
                    discrete,
                    NotifyPointerAxisDiscreteOptions::default(),
                )
                .await
                .map_err(portal_err("NotifyPointerAxisDiscrete"))
        }

        /// No-op retained for API parity with
        /// [`crate::input::VirtualInput::flush`]: every `Notify*` request is a
        /// synchronous D-Bus call already acknowledged by the portal before
        /// its own method returns, so there is nothing client-side left to
        /// flush here.
        pub async fn flush(&self) -> Result<()> {
            Ok(())
        }

        /// Refuse early when the user did not grant the device class in the
        /// Start response, instead of letting the portal reject each event.
        fn require_device(&self, device: DeviceType, what: &str) -> Result<()> {
            if self.devices.devices().contains(device) {
                Ok(())
            } else {
                Err(Error::ProtocolMissing(format!(
                    "{PORTAL}: {what} access not granted in the Start response"
                )))
            }
        }

        fn active_session(&self) -> Result<&Session<RemoteDesktop>> {
            self.session.as_ref().ok_or_else(|| {
                Error::Other("internal: remote-desktop session already closed".to_string())
            })
        }
    }

    fn key_state(press: bool) -> KeyState {
        if press {
            KeyState::Pressed
        } else {
            KeyState::Released
        }
    }

    /// The portal wire type is `i32`; keysyms/buttons fit comfortably but
    /// stay total rather than wrapping on absurd inputs.
    fn narrow_i32(value: u32, what: &str) -> Result<i32> {
        i32::try_from(value)
            .map_err(|_| Error::Other(format!("{what} {value:#x} exceeds the portal's i32 range")))
    }

    impl Drop for PortalInput {
        fn drop(&mut self) {
            let Some(session) = self.session.take() else {
                return;
            };
            // Closing is an async D-Bus round trip; hand the session to the
            // ambient runtime instead of blocking the dropper. Without a
            // runtime the portal reaps the session once our bus connection
            // disappears anyway.
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let _ = runtime.spawn(async move {
                    if let Err(e) = session.close().await {
                        tracing::debug!(error = %e, "remote-desktop session close failed");
                    }
                });
            } else {
                tracing::debug!("no tokio runtime on drop; abandoning remote-desktop session");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::PortalInput;
        use crate::error::Error;

        /// Without any session bus there cannot be a portal; connect must fail
        /// gracefully with a typed error rather than panicking or aborting.
        #[tokio::test]
        async fn connect_without_portal_degrades_gracefully() {
            // Only meaningful when there is genuinely no session bus; skip
            // silently on developer machines where a real portal would pop a
            // consent dialog mid-test.
            if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty()) {
                return;
            }
            match PortalInput::connect().await {
                Ok(_) => panic!("unexpectedly connected without a session bus"),
                Err(e) => assert!(
                    matches!(
                        e,
                        Error::ProtocolMissing(_) | Error::Other(_) | Error::Io(_)
                    ),
                    "unexpected error variant: {e}"
                ),
            }
        }

        /// Probe reports availability without ever failing or panicking.
        #[tokio::test]
        async fn probe_never_fails() {
            assert!(matches!(PortalInput::probe().await, Ok(true | false)));
        }
    }
}

/// Typechecking shim for non-unix hosts; mirrors the real API exactly.
#[cfg(not(unix))]
mod imp {
    use crate::error::{Error, Result};

    /// Inert stand-in for the real `PortalInput`.
    pub struct PortalInput;

    impl PortalInput {
        /// Always fails with [`Error::Unsupported`].
        pub async fn probe() -> Result<bool> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub async fn connect() -> Result<Self> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub async fn key(&self, _keysym: u32, _press: bool) -> Result<()> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub async fn pointer_move_relative(&self, _dx: f64, _dy: f64) -> Result<()> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub async fn button(&self, _button: u32, _press: bool) -> Result<()> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub async fn axis(&self, _vertical: bool, _steps: f64) -> Result<()> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub async fn flush(&self) -> Result<()> {
            Err(Self::unsupported())
        }

        fn unsupported() -> Error {
            Error::Unsupported("the RemoteDesktop portal is unix-only".to_string())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::PortalInput;
        use crate::error::Error;

        /// Shim methods must surface `Unsupported`, never panic (Windows-friendly).
        #[tokio::test]
        async fn shim_connect_returns_unsupported() {
            match PortalInput::connect().await {
                Ok(_) => panic!("shim connect must never succeed"),
                Err(e) => assert!(
                    matches!(e, Error::Unsupported(_)),
                    "shim must fail as Unsupported, got: {e}"
                ),
            }
        }
    }
}

pub use imp::PortalInput;
