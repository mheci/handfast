#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! Phase-4 compositor validation integration tests.
//!
//! These tests talk to a **live Wayland compositor** (headless sway /
//! weston / hyprland — see `tests/README.md` for exact launch commands).
//! Every test checks its own preconditions via the environment and returns
//! early (passes as skipped) when they are not met, so the suite stays green
//! on plain CI runners (Windows, macOS, containerized Linux without a seat).
//!
//! | Test                                    | Needs                                  |
//! |-----------------------------------------|----------------------------------------|
//! | `session_detect_on_headless_weston`     | `WAYLAND_DISPLAY` reachable            |
//! | `clipboard_roundtrip`                   | data-control-capable compositor        |
//! | `idle_inhibit_acquire_release`          | `zwp_idle_inhibit_manager_v1` or D-Bus |
//! | `virtual_input_connect_rejects_gracefully` | *no* compositor (runs everywhere)   |

/// True when a Wayland compositor socket appears reachable through the
/// process environment (`WAYLAND_DISPLAY` / `WAYLAND_SOCKET` non-empty).
/// Tests whose behavior depends on a live compositor call this first and
/// return early when it is `false`.
fn requiring_wayland() -> bool {
    ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"].iter().any(|key| {
        std::env::var(key)
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
    })
}

/// Session detection against a live headless compositor: the environment
/// points at a Wayland socket, so `detect_session` must classify the session
/// as [`SessionKind::Wayland`] and resolve a non-empty advisory protocol
/// table once a known compositor hint is visible.
///
/// The protocol table in `detect.rs` is keyed on compositor identity
/// (`SWAYSOCK`, `HYPRLAND_INSTANCE_SIGNATURE`, `XDG_CURRENT_DESKTOP`, …).
/// Bare weston exports none of those hints, so when no compositor is
/// recognized the assertion is vacuous and the test skips it rather than
/// failing on an intentionally-empty advisory list.
#[test]
#[cfg(unix)]
fn session_detect_on_headless_weston() {
    use handfast_wayland::{detect_session, SessionKind};

    if !requiring_wayland() {
        return; // no compositor socket: nothing to validate here
    }
    let info = detect_session();
    assert_eq!(
        info.kind,
        SessionKind::Wayland,
        "WAYLAND_DISPLAY is set but detection did not yield a Wayland session"
    );
    if info.compositor.is_none() {
        // Compositor unrecognized (e.g. stock weston): the advisory protocol
        // table is legitimately empty; skip the remainder of the check.
        return;
    }
    assert!(
        !info.protocols.is_empty(),
        "detected compositor {:?} must map onto a non-empty protocol table",
        info.compositor
    );
}

/// End-to-end clipboard round trip through the compositor's data-control
/// stack: publish a marker string, read it back, expect byte equality.
#[test]
#[cfg(unix)]
fn clipboard_roundtrip() {
    use handfast_wayland::clipboard::Clipboard;

    if !requiring_wayland() {
        return; // no compositor socket: nothing to validate here
    }
    const PAYLOAD: &str = "handfast-test";

    match Clipboard::set_text(PAYLOAD) {
        Ok(()) => {}
        Err(e) => panic!("Clipboard::set_text failed: {e}"),
    }
    let read = match Clipboard::get_text() {
        Ok(text) => text,
        Err(e) => panic!("Clipboard::get_text failed: {e}"),
    };
    assert_eq!(
        read.as_deref(),
        Some(PAYLOAD),
        "clipboard round trip lost or mangled the payload"
    );
}

/// Idle inhibition lifecycle: acquire a native/D-Bus inhibitor while a live
/// session exists, then drop it. Dropping must release cleanly — the whole
/// point is that teardown never panics and never wedges the connection.
#[test]
#[cfg(unix)]
fn idle_inhibit_acquire_release() {
    use handfast_wayland::idle::IdleInhibit;

    if !requiring_wayland() {
        return; // no compositor socket: nothing to validate here
    }
    let inhibitor = match IdleInhibit::acquire("handfast compositor validation") {
        Ok(guard) => guard,
        Err(e) => panic!("IdleInhibit::acquire failed: {e}"),
    };
    drop(inhibitor); // RAII release; success == no panic
}

/// Rejection path, valid on every OS: with genuinely no compositor in the
/// environment, `VirtualInput::connect` must fail with a typed error
/// ([`Error::ProtocolMissing`] or [`Error::Unsupported`]) instead of
/// panicking, hanging, or falling back to X11.
#[test]
fn virtual_input_connect_rejects_gracefully() {
    use handfast_wayland::input::VirtualInput;
    use handfast_wayland::Error;

    if requiring_wayland() {
        return; // live compositor present: rejection path not exercisable
    }
    match VirtualInput::connect() {
        Ok(_) => panic!("VirtualInput::connect succeeded without any compositor"),
        Err(e) => assert!(
            matches!(e, Error::ProtocolMissing(_) | Error::Unsupported(_)),
            "expected ProtocolMissing or Unsupported, got: {e}"
        ),
    }
}
