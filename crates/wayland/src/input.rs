//! Virtual input injection over Wayland.
//!
//! # Protocols spoken
//!
//! | Device   | Wayland protocol                                              |
//! |----------|---------------------------------------------------------------|
//! | keyboard | `zwp_virtual_keyboard_manager_v1` / `zwp_virtual_keyboard_v1` |
//! | pointer  | `zwlr_virtual_pointer_manager_v1` (wlroots)                   |
//!
//! The keyboard path is supported by Sway 1.9+, Hyprland, KDE Plasma 6 and
//! GNOME 47+; the wlr virtual-pointer path is supported by the wlroots
//! family (Sway/Hyprland/wlroots-based). GNOME and Plasma are expected to
//! use the `portal-ei` feature (RemoteDesktop portal / libei) for pointer
//! injection — see `crate::portal_ei`. There is deliberately **no** X11 /
//! XWayland / XTEST fallback anywhere in this crate.
//!
//! A `VirtualInput` owns a dedicated Wayland connection and is intentionally
//! `!Send`-friendly: xkbcommon keymap/state objects are thread-bound, so keep
//! the instance on the daemon's input thread.
//!
//! Phase note: this module is a Phase-3 skeleton wired against headless sway
//! CI; compositor matrix validation is a Phase-4 target.

/// Real implementation over a live Wayland connection.
#[cfg(all(unix, feature = "zwp-input"))]
mod imp {
    use crate::error::{Error, Result};
    use std::collections::HashMap;
    use std::io::Write as _;
    use std::os::fd::AsFd as _;
    use std::time::Instant;

    use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    };
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };
    use xkbcommon::xkb;

    /// `zwp_virtual_keyboard_v1.keymap.format`: XKB keymap, version 1.
    const KEYMAP_FORMAT_XKB_V1: u32 = 1;
    /// `zwp_virtual_keyboard_v1.key.state`: released.
    const KEY_STATE_RELEASED: u32 = 0;
    /// `zwp_virtual_keyboard_v1.key.state`: pressed.
    const KEY_STATE_PRESSED: u32 = 1;
    /// Scroll units per wheel notch for `zwlr_virtual_pointer_v1.axis`.
    const WHEEL_STEP_UNITS: f64 = 10.0;
    /// Highest `wl_seat` version we bind (all events above v5 are ignored).
    const SEAT_MAX_VERSION: u32 = 8;
    /// `zwp_virtual_keyboard_manager_v1` has exactly one version.
    const KEYBOARD_MANAGER_MAX_VERSION: u32 = 1;
    /// `zwlr_virtual_pointer_manager_v1` v2 adds `create_virtual_pointer_with_output`.
    const POINTER_MANAGER_MAX_VERSION: u32 = 2;

    /// A live virtual-input handle speaking to the compositor over its own
    /// Wayland connection.
    ///
    /// Created via [`VirtualInput::connect`]; every fallible operation
    /// degrades to an [`Error`] instead of aborting the daemon.
    pub struct VirtualInput {
        conn: Connection,
        qh: QueueHandle<Self>,
        seat: Option<wl_seat::WlSeat>,
        keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
        pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
        keyboard: Option<ZwpVirtualKeyboardV1>,
        pointer: Option<ZwlrVirtualPointerV1>,
        context: xkb::Context,
        keymap: Option<xkb::Keymap>,
        key_state: Option<xkb::State>,
        /// keysym → wire keycode memo (`None` = known-missing), keyed by raw keysym.
        keysym_cache: HashMap<u32, Option<u32>>,
        /// Monotonic base for protocol millisecond timestamps.
        epoch: Instant,
    }

    impl VirtualInput {
        /// Bind a separate Wayland connection and scan the registry for the
        /// virtual input manager globals.
        ///
        /// Errors with [`Error::Unsupported`] when no Wayland session is
        /// reachable and [`Error::ProtocolMissing`] when the compositor does
        /// not advertise *any* of the virtual input managers. Individual
        /// missing managers degrade to per-method errors so that, e.g.,
        /// keyboard-only compositors still work for keys. NEVER falls back
        /// to X11.
        pub fn connect() -> Result<Self> {
            let has_wayland_env = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
                .iter()
                .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
            if !has_wayland_env {
                return Err(Error::Unsupported(
                    "virtual input requires a Wayland session (WAYLAND_DISPLAY/WAYLAND_SOCKET unset)"
                        .to_string(),
                ));
            }

            let conn = Connection::connect_to_env()
                .map_err(|e| Error::Other(format!("wayland connect: {e}")))?;
            let mut queue = conn.new_event_queue::<VirtualInput>();
            let qh = queue.handle();

            let mut input = VirtualInput {
                conn: conn.clone(),
                qh: qh.clone(),
                seat: None,
                keyboard_manager: None,
                pointer_manager: None,
                keyboard: None,
                pointer: None,
                context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
                keymap: None,
                key_state: None,
                keysym_cache: HashMap::new(),
                epoch: Instant::now(),
            };

            // Roundtrip #1: receive wl_registry globals and bind the managers.
            conn.display().get_registry(&qh, ());
            queue
                .roundtrip(&mut input)
                .map_err(|e| Error::Other(format!("wayland registry roundtrip: {e}")))?;
            // Roundtrip #2: propagate the binds, drain initial wl_seat events.
            queue
                .roundtrip(&mut input)
                .map_err(|e| Error::Other(format!("wayland bind roundtrip: {e}")))?;
            // The event queue is dropped here on purpose: these interfaces
            // emit no events, requests go through the connection directly.

            if input.keyboard_manager.is_none() && input.pointer_manager.is_none() {
                return Err(Error::ProtocolMissing(
                    "zwp_virtual_keyboard_manager_v1, zwlr_virtual_pointer_manager_v1".to_string(),
                ));
            }
            if input.keyboard_manager.is_none() {
                tracing::warn!(
                    "compositor lacks zwp_virtual_keyboard_manager_v1; key injection disabled"
                );
            }
            if input.pointer_manager.is_none() {
                tracing::warn!(
                    "compositor lacks zwlr_virtual_pointer_manager_v1; pointer injection disabled"
                );
            }
            if input.seat.is_none() {
                tracing::warn!("no wl_seat advertised; lazy keyboard/pointer creation will fail");
            }
            Ok(input)
        }

        /// Flush pending requests to the compositor. Call periodically after
        /// bursts of input events; individual request sends are buffered by
        /// the connection and only hit the socket here.
        pub fn flush(&mut self) -> Result<()> {
            if let Some(pe) = self.conn.protocol_error() {
                self.mark_dead();
                return Err(Error::Other(format!(
                    "wayland connection dead (protocol error): {pe}"
                )));
            }
            self.conn
                .flush()
                .map_err(|e| Error::Other(format!("wayland flush: {e}")))
        }

        /// Send a keysym press/release. `keysym` is an xkb keysym
        /// (linux/input-event-codes compatible via xkbcommon); the evdev
        /// keycode is resolved through the US-qwerty keymap created lazily on
        /// first use. Modifiers derived from the tracked state are pushed
        /// after every key event so Shift/Ctrl combos behave predictably.
        pub fn key(&mut self, keysym: u32, press: bool) -> Result<()> {
            let keycode = match self.keycode_for_keysym(keysym)? {
                Some(code) => code,
                None => {
                    return Err(Error::Other(format!(
                        "keysym {keysym:#x} not present in the injected US-qwerty keymap"
                    )))
                }
            };
            let direction = if press {
                xkb::KeyDirection::Down
            } else {
                xkb::KeyDirection::Up
            };
            let (depressed, latched, locked, group) = {
                let Some(state) = self.key_state.as_mut() else {
                    return Err(Error::Other(
                        "xkb state missing after keyboard init".to_string(),
                    ));
                };
                state.update_key(xkb::Keycode::new(keycode), direction);
                (
                    state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                    state.serialize_mods(xkb::STATE_MODS_LATCHED),
                    state.serialize_mods(xkb::STATE_MODS_LOCKED),
                    state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
                )
            };

            // Clone the proxy so the shared borrow of `self` ends here.
            let keyboard = self.ensure_keyboard()?.clone();
            keyboard.key(
                self.now_ms(),
                keycode,
                if press {
                    KEY_STATE_PRESSED
                } else {
                    KEY_STATE_RELEASED
                },
            );
            keyboard.modifiers(depressed, latched, locked, group);
            Ok(())
        }

        /// Move the pointer by `(dx, dy)` in global compositor space
        /// (`zwlr_virtual_pointer_v1.motion`) and terminate the frame.
        pub fn pointer_move_relative(&mut self, dx: f64, dy: f64) -> Result<()> {
            let pointer = self.ensure_pointer()?;
            pointer.motion(self.now_ms(), dx, dy);
            pointer.frame();
            Ok(())
        }

        /// Press/release a pointer button using linux/input-event-codes values
        /// (`BTN_LEFT = 0x110`, `BTN_RIGHT = 0x111`, …).
        pub fn button(&mut self, button: u32, press: bool) -> Result<()> {
            let pointer = self.ensure_pointer()?;
            let state = if press {
                wl_pointer::ButtonState::Pressed
            } else {
                wl_pointer::ButtonState::Released
            };
            pointer.button(self.now_ms(), button, state);
            pointer.frame();
            Ok(())
        }

        /// Scroll `steps` wheel notches; positive scrolls down/right.
        /// Uses `axis_source(wheel)` + `axis` + `axis_discrete` + `frame`
        /// so both smooth-scrolling and discrete-scrolling clients react.
        pub fn axis(&mut self, vertical: bool, steps: f64) -> Result<()> {
            let notches = steps.round();
            if notches == 0.0 {
                return Ok(());
            }
            let discrete = notches.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
            let axis = if vertical {
                wl_pointer::Axis::VerticalScroll
            } else {
                wl_pointer::Axis::HorizontalScroll
            };
            let value = f64::from(discrete) * WHEEL_STEP_UNITS;
            let pointer = self.ensure_pointer()?;
            let timestamp = self.now_ms();
            pointer.axis_source(wl_pointer::AxisSource::Wheel);
            pointer.axis(timestamp, axis, value);
            pointer.axis_discrete(timestamp, axis, value, discrete);
            pointer.frame();
            Ok(())
        }

        /// Lazily create the `zwp_virtual_keyboard_v1` and upload the
        /// US-qwerty XKB keymap (serialized into an anonymous fd, per the
        /// protocol's `keymap(format, fd, size)` request).
        fn ensure_keyboard(&mut self) -> Result<ZwpVirtualKeyboardV1> {
            if self.keyboard.is_none() {
                let keyboard_manager = self.keyboard_manager.clone().ok_or_else(|| {
                    Error::ProtocolMissing("zwp_virtual_keyboard_manager_v1".to_string())
                })?;
                let seat = self.seat.clone().ok_or_else(|| {
                    Error::ProtocolMissing(
                        "wl_seat (required to attach a virtual keyboard)".to_string(),
                    )
                })?;
                let keymap = self.ensure_keymap()?;
                let keymap_text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);

                let mut keymap_fd = tempfile::tempfile()?;
                keymap_fd.write_all(keymap_text.as_bytes())?;
                keymap_fd.flush()?;

                let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &self.qh, ());
                keyboard.keymap(
                    KEYMAP_FORMAT_XKB_V1,
                    keymap_fd.as_fd(),
                    keymap_text.len() as u32,
                );
                tracing::debug!(
                    bytes = keymap_text.len(),
                    "uploaded virtual keyboard keymap"
                );
                self.keyboard = Some(keyboard);
            }
            Ok(self
                .keyboard
                .as_ref()
                .ok_or_else(|| {
                    Error::Other("internal: virtual keyboard missing after creation".to_string())
                })?
                .clone())
        }

        /// Lazily create the `zwlr_virtual_pointer_v1`.
        fn ensure_pointer(&mut self) -> Result<ZwlrVirtualPointerV1> {
            if self.pointer.is_none() {
                let pointer_manager = self.pointer_manager.clone().ok_or_else(|| {
                    Error::ProtocolMissing("zwlr_virtual_pointer_manager_v1".to_string())
                })?;
                // Seat is optional per protocol; pass it when known so the
                // compositor can attribute the device.
                let pointer =
                    pointer_manager.create_virtual_pointer(self.seat.as_ref(), &self.qh, ());
                self.pointer = Some(pointer);
            }
            Ok(self
                .pointer
                .as_ref()
                .ok_or_else(|| {
                    Error::Other("internal: virtual pointer missing after creation".to_string())
                })?
                .clone())
        }

        /// Compile (once) and return the US-qwerty keymap plus its state machine.
        fn ensure_keymap(&mut self) -> Result<xkb::Keymap> {
            if self.keymap.is_none() {
                // `options: None` defers to defaults; empty rules/model/variant
                // select libxkbcommon defaults with layout "us".
                let keymap = xkb::Keymap::new_from_names(
                    &self.context,
                    "",
                    "",
                    "us",
                    "",
                    None,
                    xkb::COMPILE_NO_FLAGS,
                )
                .ok_or_else(|| {
                    Error::Other("failed to compile US-qwerty XKB keymap".to_string())
                })?;
                let state = xkb::State::new(&keymap);
                self.key_state = Some(state);
                self.keymap = Some(keymap);
            }
            match self.keymap.as_ref() {
                Some(keymap) => Ok(keymap.clone()),
                None => Err(Error::Other(
                    "internal: keymap missing after compilation".to_string(),
                )),
            }
        }

        /// Resolve a keysym to a wire keycode (XKB keycode == evdev + 8)
        /// using level-0 symbols of the compiled keymap; results are memoized.
        fn keycode_for_keysym(&mut self, keysym_raw: u32) -> Result<Option<u32>> {
            if let Some(cached) = self.keysym_cache.get(&keysym_raw) {
                return Ok(*cached);
            }
            let keymap = self.ensure_keymap()?;
            let target = xkb::Keysym::new(keysym_raw);
            let mut found: Option<xkb::Keycode> = None;
            keymap.key_for_each(|km, keycode| {
                if found.is_some() {
                    return;
                }
                if km.key_get_syms_by_level(keycode, 0, 0).contains(&target) {
                    found = Some(keycode);
                }
            });
            let resolved = found.map(|keycode| keycode.raw());
            self.keysym_cache.insert(keysym_raw, resolved);
            Ok(resolved)
        }

        fn now_ms(&self) -> u32 {
            self.epoch.elapsed().as_millis().min(u128::from(u32::MAX)) as u32
        }

        /// Marks cached proxies unusable after fatal connection errors.
        fn mark_dead(&mut self) {
            self.keyboard = None;
            self.pointer = None;
        }
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for VirtualInput {
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
                "wl_seat" => {
                    state.seat = Some(registry.bind(name, version.min(SEAT_MAX_VERSION), qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.keyboard_manager = Some(registry.bind(
                        name,
                        version.min(KEYBOARD_MANAGER_MAX_VERSION),
                        qh,
                        (),
                    ));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.pointer_manager =
                        Some(registry.bind(name, version.min(POINTER_MANAGER_MAX_VERSION), qh, ()));
                }
                other => {
                    tracing::trace!(interface = other, "ignoring wayland global");
                }
            }
        }
    }

    impl Dispatch<wl_seat::WlSeat, ()> for VirtualInput {
        fn event(
            _: &mut Self,
            _: &wl_seat::WlSeat,
            event: wl_seat::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                wl_seat::Event::Capabilities {
                    capabilities: WEnum::Value(caps),
                } => {
                    // Virtual input devices do not require seat capabilities.
                    tracing::trace!(?caps, "seat capabilities");
                }
                wl_seat::Event::Name { .. } => {}
                _ => {}
            }
        }
    }

    impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for VirtualInput {
        fn event(
            _: &mut Self,
            _: &ZwpVirtualKeyboardManagerV1,
            _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwpVirtualKeyboardV1, ()> for VirtualInput {
        fn event(
            _: &mut Self,
            _: &ZwpVirtualKeyboardV1,
            _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for VirtualInput {
        fn event(
            _: &mut Self,
            _: &ZwlrVirtualPointerManagerV1,
            _: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwlrVirtualPointerV1, ()> for VirtualInput {
        fn event(
            _: &mut Self,
            _: &ZwlrVirtualPointerV1,
            _: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Drop for VirtualInput {
        fn drop(&mut self) {
            if let Some(keyboard) = &self.keyboard {
                keyboard.destroy();
            }
            if let Some(pointer) = &self.pointer {
                pointer.destroy();
            }
            if let Some(pointer_manager) = &self.pointer_manager {
                pointer_manager.destroy();
            }
            if let Err(e) = self.conn.flush() {
                tracing::debug!(error = %e, "wayland flush during VirtualInput drop failed");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::VirtualInput;
        use crate::error::Error;

        /// Without a compositor the constructor must fail gracefully with a
        /// typed error rather than panicking or aborting.
        #[test]
        fn connect_without_compositor_degrades_gracefully() {
            // Only meaningful when there is genuinely no Wayland environment;
            // skip silently inside compositor CI jobs where one may exist.
            if std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var_os("WAYLAND_SOCKET").is_some()
            {
                return;
            }
            match VirtualInput::connect() {
                Ok(_) => panic!("unexpectedly connected without a compositor"),
                Err(e) => assert!(
                    matches!(
                        e,
                        Error::Unsupported(_)
                            | Error::ProtocolMissing(_)
                            | Error::Other(_)
                            | Error::Io(_)
                    ),
                    "unexpected error variant: {e}"
                ),
            }
        }
    }
}

/// Typechecking shim for non-Wayland platforms (Windows/macOS CI) or when the
/// `zwp-input` feature is disabled. Every method reports
/// [`Error::Unsupported`]; signatures mirror the real implementation exactly.
#[cfg(not(all(unix, feature = "zwp-input")))]
mod imp {
    use crate::error::{Error, Result};

    /// Inert stand-in for the real `VirtualInput`; never touches any display
    /// server. See the `zwp-input`-enabled variant for documentation.
    pub struct VirtualInput {
        _private: (),
    }

    impl VirtualInput {
        /// Always fails with [`Error::Unsupported`] off-Wayland builds.
        pub fn connect() -> Result<Self> {
            Err(Error::Unsupported(
                "virtual input requires a Wayland session".to_string(),
            ))
        }

        /// Always fails with [`Error::Unsupported`].
        pub fn flush(&mut self) -> Result<()> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub fn key(&mut self, _keysym: u32, _press: bool) -> Result<()> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub fn pointer_move_relative(&mut self, _dx: f64, _dy: f64) -> Result<()> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub fn button(&mut self, _button: u32, _press: bool) -> Result<()> {
            Err(Self::unsupported())
        }

        /// Always fails with [`Error::Unsupported`].
        pub fn axis(&mut self, _vertical: bool, _steps: f64) -> Result<()> {
            Err(Self::unsupported())
        }

        fn unsupported() -> Error {
            Error::Unsupported("virtual input requires a Wayland session".to_string())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::VirtualInput;
        use crate::error::Error;

        /// Shim methods must surface `Unsupported`, never panic (Windows-friendly).
        #[test]
        fn shim_connect_returns_unsupported() {
            match VirtualInput::connect() {
                Ok(_) => panic!("shim connect must never succeed"),
                Err(e) => assert!(
                    matches!(e, Error::Unsupported(_)),
                    "shim must fail as Unsupported, got: {e}"
                ),
            }
        }
    }
}

pub use imp::VirtualInput;
