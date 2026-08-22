//! Clipboard access and change watching over data-control protocols.
//!
//! # Protocols spoken
//!
//! | Operation     | Wayland protocol                                                                  |
//! |---------------|-----------------------------------------------------------------------------------|
//! | get/set text  | `ext_data_control_v1` **and** `wlr_data_control_unstable_v1` (via wl-clipboard-rs) |
//! | watch changes | `ext_data_control_device_manager_v1`, falling back to `zwlr_data_control_device_manager_v1` |
//!
//! `wl-clipboard-rs` implements both data-control stacks and picks the right
//! one automatically for get/set. Compositor matrix (Phase-4 validation
//! targets): KDE Plasma 6, GNOME 48+ (`ext` stack), Sway 1.9+, Hyprland
//! (`zwlr` stack).
//!
//! The watcher is fully event-driven (CONTRIBUTING.md rule 4): a dedicated
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
    /// Both data-control stacks expose primary selection at version 2.
    const MANAGER_MAX_VERSION: u32 = 2;

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
        /// `zwlr_data_control_device_manager_v1` is advertised.
        pub fn watch_text() -> Result<tokio::sync::mpsc::UnboundedReceiver<String>> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            spawn_watcher(tx)?;
            Ok(rx)
        }
    }

    /// Read TEXT from one clipboard through wl-clipboard-rs.
    fn read_clipboard(
        clipboard: wl_clipboard_rs::paste::ClipboardType,
    ) -> std::result::Result<Option<String>, wl_clipboard_rs::paste::Error> {
        use wl_clipboard_rs::paste::{self};
        match paste::get_contents(clipboard, paste::Seat::Unspecified, paste::MimeType::Text) {
            Ok((mut reader, _mime)) => {
                let mut buffer = Vec::new();
                reader.read_to_end(&mut buffer)?;
                Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
            }
            Err(paste::Error::NoSeats | paste::Error::ClipboardEmpty | paste::Error::NoMimeType) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Per-watcher dedupe caches plus emission logic, shared by both backends.
    #[derive(Default)]
    struct Core {
        last_regular: Option<String>,
        last_primary: Option<String>,
    }

    impl Core {
        /// Snapshot current selections without emitting (startup baseline).
        fn prime(&mut self) {
            use wl_clipboard_rs::paste::ClipboardType;
            self.last_regular = read_clipboard(ClipboardType::Regular).ok().flatten();
            self.last_primary = read_clipboard(ClipboardType::Primary).ok().flatten();
        }

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

    /// Instantiate the preferred backend (`ext` first, `wlr` fallback) and
    /// move its dispatch loop onto a dedicated thread.
    fn spawn_watcher(tx: tokio::sync::mpsc::UnboundedSender<String>) -> Result<()> {
        ensure_wayland_env()?;
        let watcher = match ext_backend::Watcher::start(tx.clone()) {
            Ok(watcher) => watcher,
            Err(Error::ProtocolMissing(missing)) => {
                tracing::debug!(
                    missing,
                    "ext-data-control unavailable; falling back to wlr-data-control"
                );
                wlr_backend::Watcher::start(tx)?
            }
            Err(e) => return Err(e),
        };
        std::thread::Builder::new()
            .name("handfast-clipboard-watch".to_string())
            .spawn(move || watcher.run())?;
        Ok(())
    }

    /// One data-control backend: registry scan, per-seat devices, and a
    /// blocking dispatch loop forwarding selection changes.
    ///
    /// Parameters: backend module name, advertised global name, then
    /// (module path, type name) pairs for the manager/device/offer interfaces.
    macro_rules! define_watch_backend {
        (
            $mod_name:ident,
            $iface_name:expr,
            $manager_mod:path,
            $manager_ty:ident,
            $device_mod:path,
            $device_ty:ident,
            $offer_mod:path,
            $offer_ty:ident
        ) => {
            mod $mod_name {
                use super::{ensure_wayland_env, Core, MANAGER_MAX_VERSION, SEAT_MAX_VERSION};
                use crate::error::{Error, Result};
                use wayland_client::protocol::{wl_registry, wl_seat};
                use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
                use $device_mod::Event as DeviceEvent;
                use $manager_mod::$manager_ty;
                use $device_mod::$device_ty;
                use $offer_mod::$offer_ty;

                /// Registry scan results plus watcher plumbing.
                pub(super) struct State {
                    tx: tokio::sync::mpsc::UnboundedSender<String>,
                    core: Core,
                    dirty: bool,
                    manager: Option<$manager_ty>,
                    seats: Vec<wl_seat::WlSeat>,
                }

                /// Owned pieces of the watcher thread's connection.
                pub(super) struct Watcher {
                    conn: Connection,
                    queue: EventQueue<State>,
                    state: State,
                }

                impl Watcher {
                    pub(super) fn start(
                        tx: tokio::sync::mpsc::UnboundedSender<String>,
                    ) -> Result<Self> {
                        ensure_wayland_env()?;
                        let conn = Connection::connect_to_env()
                            .map_err(|e| Error::Other(format!("wayland connect: {e}")))?;
                        let mut queue = conn.new_event_queue::<State>();
                        let qh = queue.handle();
                        let mut state = State {
                            tx,
                            core: Core::default(),
                            dirty: false,
                            manager: None,
                            seats: Vec::new(),
                        };

                        conn.display().get_registry(&qh, ());
                        queue.roundtrip(&mut state).map_err(|e| {
                            Error::Other(format!("wayland registry roundtrip: {e}"))
                        })?;
                        queue.roundtrip(&mut state).map_err(|e| {
                            Error::Other(format!("wayland bind roundtrip: {e}"))
                        })?;

                        let manager =
                            state.manager.clone().ok_or_else(|| {
                                Error::ProtocolMissing($iface_name.to_string())
                            })?;
                        if state.seats.is_empty() {
                            return Err(Error::ProtocolMissing("wl_seat".to_string()));
                        }
                        // One data-control device per advertised seat. Device
                        // handles need not be retained: events stay routable
                        // through the object data attached at creation.
                        let seats = std::mem::take(&mut state.seats);
                        for seat in &seats {
                            manager.get_data_device(seat, &qh, ());
                        }
                        state.seats = seats;
                        queue.roundtrip(&mut state).map_err(|e| {
                            Error::Other(format!("wayland device roundtrip: {e}"))
                        })?;
                        state.core.prime();
                        Ok(Self { conn, queue, state })
                    }

                    /// Blocking dispatch loop; exits when the receiver drops
                    /// or the compositor connection fails.
                    pub(super) fn run(mut self) {
                        loop {
                            if self.state.tx.is_closed() {
                                break;
                            }
                            if let Err(e) = self.queue.blocking_dispatch(&mut self.state) {
                                tracing::warn!(error = %e, "clipboard watcher dispatch failed");
                                break;
                            }
                            if std::mem::take(&mut self.state.dirty) {
                                self.state.core.emit_changes(&self.state.tx);
                            }
                        }
                        if let Err(e) = self.conn.flush() {
                            tracing::debug!(error = %e, "clipboard watcher flush on exit failed");
                        }
                    }
                }

                impl Dispatch<wl_registry::WlRegistry, ()> for State {
                    fn event(
                        state: &mut Self,
                        registry: &wl_registry::WlRegistry,
                        event: wl_registry::Event,
                        _: &(),
                        _: &Connection,
                        qh: &QueueHandle<Self>,
                    ) {
                        let wl_registry::Event::Global { name, interface, version } = event else {
                            return;
                        };
                        match interface.as_str() {
                            "wl_seat" => {
                                let seat: wl_seat::WlSeat =
                                    registry.bind(name, version.min(SEAT_MAX_VERSION), qh, ());
                                state.seats.push(seat);
                            }
                            found if found == $iface_name => {
                                let manager: $manager_ty =
                                    registry.bind(name, version.min(MANAGER_MAX_VERSION), qh, ());
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
                        _: &mut Self,
                        _: &wl_seat::WlSeat,
                        _: wl_seat::Event,
                        _: &(),
                        _: &Connection,
                        _: &QueueHandle<Self>,
                    ) {
                    }
                }

                impl Dispatch<$manager_ty, ()> for State {
                    fn event(
                        _: &mut Self,
                        _: &$manager_ty,
                        _: $manager_mod::$manager_ty::Event,
                        _: &(),
                        _: &Connection,
                        _: &QueueHandle<Self>,
                    ) {
                    }
                }

                impl Dispatch<$device_ty, ()> for State {
                    fn event(
                        state: &mut Self,
                        _: &$device_ty,
                        event: DeviceEvent,
                        _: &(),
                        _: &Connection,
                        _: &QueueHandle<Self>,
                    ) {
                        // Field names are intentionally not bound: any
                        // selection/primary-selection/finished event means the
                        // selection changed; data_offer payloads are ignored
                        // because content is re-read via wl-clipboard-rs.
                        match event {
                            DeviceEvent::Selection { .. }
                            | DeviceEvent::PrimarySelection { .. }
                            | DeviceEvent::Finished => state.dirty = true,
                            _ => {}
                        }
                    }

                    // The `data_offer` event creates child offer objects; tell
                    // the queue which type/userdata to instantiate them with.
                    wayland_client::event_created_child!(
                        State,
                        $device_ty,
                        [
                            $device_mod::EVT_DATA_OFFER_OPCODE = ($offer_ty, ())
                        ]
                    );
                }

                impl Dispatch<$offer_ty, ()> for State {
                    fn event(
                        _: &mut Self,
                        _: &$offer_ty,
                        _: $offer_mod::$offer_ty::Event,
                        _: &(),
                        _: &Connection,
                        _: &QueueHandle<Self>,
                    ) {
                    }
                }
            }
        };
    }

    define_watch_backend!(
        ext_backend,
        "ext_data_control_device_manager_v1",
        wayland_protocols::ext::data_control::v1::client::ext_data_control_device_manager_v1,
        ExtDataControlDeviceManagerV1,
        wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1,
        ExtDataControlDeviceV1,
        wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1,
        ExtDataControlOfferV1
    );

    define_watch_backend!(
        wlr_backend,
        "zwlr_data_control_device_manager_v1",
        wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_manager_v1,
        ZwlrDataControlDeviceManagerV1,
        wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1,
        ZwlrDataControlDeviceV1,
        wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1,
        ZwlrDataControlOfferV1
    );

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
                Err(Error::Unsupported(_) | Error::ProtocolMissing(_) | Error::Other(_)) => Ok(()),
                Ok(_) => Err(Error::Other("watch unexpectedly succeeded headless".to_string())),
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
