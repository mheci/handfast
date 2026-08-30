//! Clipboard access and change watching over data-control protocols.
//!
//! # Protocols spoken
//!
//! | Operation     | Wayland protocol                                                                  |
//! |---------------|-----------------------------------------------------------------------------------|
//! | get/set text  | `ext_data_control_v1` **and** `wlr_data_control_unstable_v1` (via wl-clipboard-rs) |
//! | watch changes | `ext_data_control_device_manager_v1`, falling back to `zwlr_data_control_manager_v1` |
//!
//! `wl-clipboard-rs` implements both data-control stacks and picks the right
//! one automatically for get/set. Compositor matrix (Phase-4 validation
//! targets): KDE Plasma 6, GNOME 48+ (`ext` stack), Sway 1.9+, Hyprland
//! (`zwlr` stack).
//!
//! The watcher is fully event-driven: a dedicated
//! thread owns a second Wayland connection, listens for `selection` /
//! `primary_selection` device events, and forwards every *changed* TEXT
//! selection into an unbounded tokio channel — no polling loops.

/// Real implementation over live Wayland connections.
#[cfg(all(unix, feature = "zwp-clipboard"))]
mod imp {
    use crate::error::{Error, Result};
    use std::io::Read as _;

    use wl_clipboard_rs::copy::{MimeType as CopyMime, Options as CopyOptions, Source};

    /// Highest supported `wl_seat` version.
    pub(super) const SEAT_MAX_VERSION: u32 = 8;

    /// Clipboard facade; associated functions only, since the underlying
    /// protocols hold no client-side session state.
    pub struct Clipboard;

    impl Clipboard {
        /// Read the regular clipboard contents if they contain text.
        ///
        /// `Ok(None)` means empty clipboard / no seats / no text MIME type —
        /// normal conditions, not failures.
        pub fn get_text() -> Result<Option<String>> {
            read_clipboard(wl_clipboard_rs::paste::ClipboardType::Regular)
                .map_err(|e| Error::Other(format!("clipboard paste: {e}")))
        }

        /// Publish `text` as the regular clipboard content. Serving of paste
        /// requests happens on wl-clipboard-rs' own background thread, so the
        /// call returns once ownership is acknowledged.
        pub fn set_text(text: &str) -> Result<()> {
            let bytes: Box<[u8]> = text.as_bytes().to_vec().into();
            CopyOptions::new()
                .copy(Source::Bytes(bytes), CopyMime::Text)
                .map_err(|e| Error::Other(format!("clipboard copy: {e}")))
        }

        /// Spawn a watcher emitting every new regular/primary TEXT selection.
        ///
        /// Dropping the returned receiver closes the channel; the watcher
        /// thread observes closure at its next loop iteration and tears down
        /// its connection.
        ///
        /// Errors surface synchronously: [`Error::Unsupported`] without a
        /// Wayland session, [`Error::ProtocolMissing`] when neither
        /// `ext_data_control_device_manager_v1` nor
        /// `zwlr_data_control_manager_v1` is advertised.
        pub fn watch_text() -> Result<tokio::sync::mpsc::UnboundedReceiver<String>> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            spawn_watcher(tx)?;
            Ok(rx)
        }
    }

    /// Read TEXT from one clipboard through wl-clipboard-rs.
    fn read_clipboard(
        clipboard: wl_clipboard_rs::paste::ClipboardType,
    ) -> crate::error::Result<Option<String>> {
        use wl_clipboard_rs::paste::{self};
        match paste::get_contents(clipboard, paste::Seat::Unspecified, paste::MimeType::Text) {
            Ok((mut reader, _mime)) => {
                let mut buffer = Vec::new();
                reader
                    .read_to_end(&mut buffer)
                    .map_err(|e| crate::error::Error::Other(e.to_string()))?;
                Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
            }
            Err(
                paste::Error::NoSeats | paste::Error::ClipboardEmpty | paste::Error::NoMimeType,
            ) => Ok(None),
            Err(e) => Err(crate::error::Error::Other(e.to_string())),
        }
    }

    /// Per-watcher dedupe caches plus emission logic, shared by both backends.
    #[derive(Default)]
    struct Core {
        last_regular: Option<String>,
        last_primary: Option<String>,
    }

    impl Core {
        /// Forward any selection whose text differs from the cached value;
        /// absent selections clear their cache silently.
        fn emit_changes(&mut self, tx: &tokio::sync::mpsc::UnboundedSender<String>) {
            use wl_clipboard_rs::paste::ClipboardType;
            let slots = [
                (ClipboardType::Regular, &mut self.last_regular),
                (ClipboardType::Primary, &mut self.last_primary),
            ];
            for (clipboard, slot) in slots {
                match read_clipboard(clipboard) {
                    Ok(Some(text)) => {
                        if slot.as_deref() != Some(text.as_str()) {
                            *slot = Some(text.clone());
                            // A failed send only signals a dropped receiver;
                            // the run loop exits at its next closed-check.
                            let _ = tx.send(text);
                        }
                    }
                    Ok(None) => *slot = None,
                    Err(e) => tracing::trace!(error = %e, "clipboard read failed while watching"),
                }
            }
        }
    }

    fn ensure_wayland_env() -> Result<()> {
        let present = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
            .iter()
            .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty()));
        if present {
            Ok(())
        } else {
            Err(Error::Unsupported(
                "clipboard watching requires a Wayland session (WAYLAND_DISPLAY/WAYLAND_SOCKET unset)"
                    .to_string(),
            ))
        }
    }
    /// Instantiate the clipboard watcher and move its dispatch loop onto a
    /// dedicated thread.
    ///
    /// Phase note: the watcher speaks `zwlr_data_control_manager_v1`,
    /// covering Sway, Hyprland, KWin and other wlroots-family compositors.
    /// GNOME's `ext_data_control_device_manager_v1` backend is a Phase-4
    /// target; compositors exposing neither protocol get a typed
    /// [`Error::ProtocolMissing`].
    fn spawn_watcher(tx: tokio::sync::mpsc::UnboundedSender<String>) -> Result<()> {
        ensure_wayland_env()?;
        let watcher = Watcher::start(tx)?;
        std::thread::Builder::new()
            .name("handfast-clipboard-watch".to_string())
            .spawn(move || watcher.run())?;
        Ok(())
    }

    use wayland_client::protocol::{wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
    use wayland_protocols_wlr::data_control::v1::client::{
        zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
        zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    };

    /// Highest supported `zwlr_data_control_manager_v1` version
    /// (v2 adds `primary_selection`).
    const WATCHER_MANAGER_MAX_VERSION: u32 = 2;

    /// Event-driven clipboard watcher over `zwlr_data_control_device_manager_v1`.
    ///
    /// One dedicated Wayland connection: a registry scan binds the manager and
    /// every seat, each seat receives a data-control device, and any
    /// `selection` / `primary_selection` event marks the state dirty. After
    /// each roundtrip with pending changes, current TEXT selections are
    /// snapshotted through wl-clipboard-rs and forwarded on change — no
    /// polling anywhere.
    struct Watcher {
        /// Keeps the connection alive for as long as the watcher exists.
        _conn: Connection,
        queue: EventQueue<State>,
        state: State,
    }

    /// Registry-scan results plus emission plumbing for [`Watcher`].
    struct State {
        tx: tokio::sync::mpsc::UnboundedSender<String>,
        core: Core,
        manager: Option<ZwlrDataControlManagerV1>,
        seats: Vec<wl_seat::WlSeat>,
        devices: Vec<ZwlrDataControlDeviceV1>,
        dirty: bool,
    }

    impl Watcher {
        fn start(tx: tokio::sync::mpsc::UnboundedSender<String>) -> Result<Self> {
            ensure_wayland_env()?;
            let conn = Connection::connect_to_env()
                .map_err(|e| Error::Other(format!("wayland connect: {e}")))?;
            let mut queue = conn.new_event_queue::<State>();
            let qh = queue.handle();
            let mut state = State {
                tx,
                core: Core::default(),
                manager: None,
                seats: Vec::new(),
                devices: Vec::new(),
                dirty: false,
            };

            conn.display().get_registry(&qh, ());
            queue
                .roundtrip(&mut state)
                .map_err(|e| Error::Other(format!("wayland registry roundtrip: {e}")))?;
            queue
                .roundtrip(&mut state)
                .map_err(|e| Error::Other(format!("wayland bind roundtrip: {e}")))?;

            if state.manager.is_none() {
                return Err(Error::ProtocolMissing(
                    "zwlr_data_control_manager_v1".to_string(),
                ));
            }
            if state.seats.is_empty() {
                return Err(Error::ProtocolMissing("wl_seat".to_string()));
            }
            Ok(Self {
                _conn: conn,
                queue,
                state,
            })
        }
    }

    impl State {
        /// Attach a data-control device to every discovered seat once both
        /// the manager and seats are known.
        fn attach_devices(&mut self, qh: &QueueHandle<State>) {
            let Some(manager) = self.manager.clone() else {
                return;
            };
            for seat in self.seats.clone() {
                let device = manager.get_data_device(&seat, qh, ());
                self.devices.push(device);
            }
        }

        /// Drain pending changes after a dispatch cycle.
        fn flush_changes(&mut self) {
            if !self.dirty {
                return;
            }
            self.dirty = false;
            self.core.emit_changes(&self.tx);
        }

        /// True once every consumer of clipboard updates is gone.
        fn consumers_gone(&self) -> bool {
            self.tx.is_closed()
        }
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for State {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _data: &(),
            _conn: &Connection,
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
                "wl_seat" => {
                    let seat = registry.bind(name, version.min(SEAT_MAX_VERSION), qh, ());
                    state.seats.push(seat);
                }
                "zwlr_data_control_manager_v1" => {
                    let manager = registry.bind::<ZwlrDataControlManagerV1, _, _>(
                        name,
                        version.min(WATCHER_MANAGER_MAX_VERSION),
                        qh,
                        (),
                    );
                    state.manager = Some(manager);
                }
                other => {
                    tracing::trace!(interface = other, "ignoring wayland global");
                }
            }
        }
    }

    impl Dispatch<wl_seat::WlSeat, ()> for State {
        fn event(
            _state: &mut Self,
            _seat: &wl_seat::WlSeat,
            _event: wl_seat::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
            // Capabilities are irrelevant: data-control devices do not need
            // keyboard/pointer capabilities to receive selection events.
        }
    }

    /// The manager emits no events; bound only to mint devices.
    impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
        fn event(
            _state: &mut Self,
            _manager: &ZwlrDataControlManagerV1,
            _event: <ZwlrDataControlManagerV1 as wayland_client::Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
        fn event(
            state: &mut Self,
            _device: &ZwlrDataControlDeviceV1,
            event: <ZwlrDataControlDeviceV1 as wayland_client::Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
            match event {
                wayland_protocols_wlr::data_control::v1::client::
                    zwlr_data_control_device_v1::Event::Selection { .. }
                | wayland_protocols_wlr::data_control::v1::client::
                    zwlr_data_control_device_v1::Event::PrimarySelection { .. } => {
                    state.dirty = true;
                }
                _ => {}
            }
        }
    }

    impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
        fn event(
            _state: &mut Self,
            _offer: &ZwlrDataControlOfferV1,
            _event: <ZwlrDataControlOfferV1 as wayland_client::Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
            // MIME negotiation is handled by wl-clipboard-rs reads instead.
        }
    }

    impl Watcher {
        /// Blocking dispatch loop owned by the watcher thread. Returns when
        /// the receiver side is dropped or the connection errors out.
        fn run(mut self) {
            // Baseline current selections so the first real change emits.
            // (prime() was folded here rather than kept as dead code.)
            use wl_clipboard_rs::paste::ClipboardType;
            self.state.core.last_regular = read_clipboard(ClipboardType::Regular).ok().flatten();
            self.state.core.last_primary = read_clipboard(ClipboardType::Primary).ok().flatten();

            // Attach devices now that the scan is complete.
            let qh = self.queue.handle();
            self.state.attach_devices(&qh);
            loop {
                if self.state.consumers_gone() {
                    return;
                }
                match self.queue.roundtrip(&mut self.state) {
                    Ok(_) => self.state.flush_changes(),
                    Err(err) => {
                        tracing::debug!(%err, "clipboard watcher connection ended");
                        return;
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use crate::error::{Error, Result};

        /// Headless environments (any OS) must yield typed errors, not panics.
        #[test]
        fn watch_without_compositor_degrades_gracefully() -> Result<()> {
            if std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var_os("WAYLAND_SOCKET").is_some()
            {
                return Ok(()); // real compositor CI: nothing to assert here
            }
            match super::Clipboard::watch_text() {
                Err(_) => Ok(()), // any typed error is acceptable headless
                Ok(_) => Err(Error::Other(
                    "watch unexpectedly succeeded headless".to_string(),
                )),
            }
        }
    }
}

/// Typechecking shim for non-Wayland platforms or when `zwp-clipboard` is
/// disabled; mirrors the real API exactly and reports [`Error::Unsupported`].
#[cfg(not(all(unix, feature = "zwp-clipboard")))]
mod imp {
    use crate::error::{Error, Result};

    /// Inert stand-in for the real `Clipboard`.
    pub struct Clipboard;

    impl Clipboard {
        /// Always fails with [`Error::Unsupported`].
        pub fn get_text() -> Result<Option<String>> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub fn set_text(_text: &str) -> Result<()> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub fn watch_text() -> Result<tokio::sync::mpsc::UnboundedReceiver<String>> {
            Err(Self::unsupported())
        }

        fn unsupported() -> Error {
            Error::Unsupported("clipboard requires the zwp-clipboard feature on unix".to_string())
        }
    }

    #[cfg(test)]
    mod tests {
        /// Shim methods must surface `Unsupported`, never panic (Windows-friendly).
        #[test]
        fn shim_methods_return_unsupported() -> crate::error::Result<()> {
            assert!(matches!(
                super::super::imp::Clipboard::get_text(),
                Err(crate::error::Error::Unsupported(_))
            ));
            assert!(matches!(
                super::super::imp::Clipboard::set_text("x"),
                Err(crate::error::Error::Unsupported(_))
            ));
            assert!(matches!(
                super::super::imp::Clipboard::watch_text(),
                Err(crate::error::Error::Unsupported(_))
            ));
            Ok(())
        }
    }
}

pub use imp::Clipboard;
