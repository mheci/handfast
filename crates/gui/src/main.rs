#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Handfast GUI front-end.
//!
//! GUI speaks only handfast-ipc over UDS; Wayland constraints live in the
//! daemon. This crate contains no display-server code: the
//! window runs through Iced's winit backend, which uses Wayland natively
//! whenever a Wayland compositor is present.

mod app;
mod bridge;
mod model;
mod view;

use crate::app::HandfastApp;

/// Fixed dark theme; a named fn item satisfies the higher-ranked
/// `Fn(&State) -> Theme` bound expected by the Application builder.
fn dark_theme(_app: &HandfastApp) -> iced::Theme {
    iced::Theme::Dark
}

fn main() -> iced::Result {
    // Iced 0.14 builder style: boot/update/view functions plus declarative
    // window configuration; `.run()` boots the state and starts the event
    // loop. The winit backend picks Wayland automatically when available.
    iced::application(HandfastApp::new, HandfastApp::update, view::root)
        .title("Handfast")
        .theme(dark_theme)
        .subscription(HandfastApp::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(960.0, 640.0),
            ..Default::default()
        })
        .centered()
        .run()
}
