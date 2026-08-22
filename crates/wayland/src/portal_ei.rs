//! Mediated input via the XDG RemoteDesktop portal (libei-style), the
//! preferred path on compositors that restrict direct virtual-input
//! protocols — notably GNOME.
//!
//! # Protocols spoken
//!
//! | Layer    | Interface                                                              |
//! |----------|-------------------------------------------------------------------------|
//! | portal   | `org.freedesktop.portal.RemoteDesktop` (`CreateSession`, `SelectDevices`, `Start`, `ConnectToEIS`) |
//! | transport| libei / input-capture emulation over the portal-provided EIS socket     |
//!
//! Compositor matrix: GNOME 47+ (primary target; Mutter prefers libei),
//! KDE Plasma 6 (secondary). **Phase-3 skeleton**: `probe` and the full
//! consent-dialog handshake are implemented; continuous event streaming
//! (`ConnectToEIS` + libei serialization) lands in Phase-4, so injection
//! methods currently return [`Error::Unsupported`].
//!
//! Unlike `input::VirtualInput` this path never touches compositor globals:
//! everything flows through the portal D-Bus service, subject to user
//! consent. There is no X11/XTEST fallback anywhere.
use crate::error::{Error, Result};

/// Real skeleton over ashpd's RemoteDesktop wrapper.
#[cfg(all(unix, feature = "portal-ei"))]
mod imp {
    use crate::error::{Error, Result};

    use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions};

    /// Handle for a mediated input session (Phase-3: handshake only).
    pub struct PortalInput;

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

        /// Run the full portal handshake: create a session, request keyboard
        /// and pointer devices, and start the session — which surfaces the
        /// desktop's user-consent dialog.
        ///
        /// Phase-3 scope: the handshake is validated and then the session is
        /// released again. Phase-4 replaces this tail with
        /// `RemoteDesktop::connect_to_eis()` and keeps the returned EIS fd
        /// alive inside this handle for libei event streaming.
        pub async fn connect() -> Result<Self> {
            let proxy = RemoteDesktop::new().await.map_err(portal_err("connect"))?;
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
                devices = ?selected.devices(),
                "remote-desktop session started"
            );
            // Skeleton: release the session until event streaming exists.
            let _ = session.close().await;
            Ok(Self)
        }

        /// Phase-4: keysym injection through the EIS stream.
        pub async fn key(&self, _keysym: u32, _press: bool) -> Result<()> {
            Err(Error::Unsupported(
                "portal-ei key streaming lands in Phase-4".to_string(),
            ))
        }

        /// Phase-4: relative pointer motion through the EIS stream.
        pub async fn pointer_move_relative(&self, _dx: f64, _dy: f64) -> Result<()> {
            Err(Error::Unsupported(
                "portal-ei pointer streaming lands in Phase-4".to_string(),
            ))
        }

        /// Phase-4: button events through the EIS stream.
        pub async fn button(&self, _button: u32, _press: bool) -> Result<()> {
            Err(Error::Unsupported(
                "portal-ei button streaming lands in Phase-4".to_string(),
            ))
        }

        /// Phase-4: scroll axis events through the EIS stream.
        pub async fn axis(&self, _vertical: bool, _steps: f64) -> Result<()> {
            Err(Error::Unsupported(
                "portal-ei axis streaming lands in Phase-4".to_string(),
            ))
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
        /// Always reports `false` availability via [`Error::Unsupported`].
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

        fn unsupported() -> Error {
            Error::Unsupported("the RemoteDesktop portal is unix-only".to_string())
        }
    }
}

pub use imp::PortalInput;
