//! Handfast GUI front-end.
//!
//! GUI speaks only handfast-ipc over UDS; Wayland constraints live in the
//! daemon (see docs/IPC.md). This crate contains no display-server code: the
//! window runs through Iced's winit backend, which uses Wayland natively
//! whenever a Wayland compositor is present.

mod app;
mod bridge;
mod model;
mod view;

use crate::app::HandfastApp;
use crate::view;

fn main() -> iced::Result {
    // Iced 0.14 builder style: boot/update/view functions plus declarative
    // window configuration; `.run()` boots the state and starts the event
    // loop. The winit backend picks Wayland automatically when available.
    iced::application(HandfastApp::new, HandfastApp::update, view::root)
        .title("Handfast")
        .theme(|_| iced::Theme::Dark)
        .subscription(HandfastApp::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(960.0, 640.0),
            ..Default::default()
        })
        .centered()
        .run()
}
