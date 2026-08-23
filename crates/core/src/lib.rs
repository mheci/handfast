#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Handfast core primitives.
//!
//! This crate provides the foundational building blocks shared by every other
//! Handfast component:
//!
//! * [`error`] — the crate-wide error type.
//! * [`paths`] — XDG-aware resolution of config/data/cache/runtime directories.
//! * [`bus`] — an in-process broadcast event bus connecting subsystems to UIs.
//! * [`store`] — an SQLite-backed device registry and key/value store, plus a
//!   crash-safe [`atomic_write`](store::atomic_write) helper.
//! * [`supervise`] — a small supervision tree that restarts crashed tasks with
//!   exponential backoff.
//!
//! Everything here is platform independent apart from a few Unix-specific
//! filesystem details that degrade gracefully on other platforms; the whole
//! crate typechecks on non-Unix hosts so cross-compilation stays trivial.

#![forbid(unsafe_code)]

/// Canonical application name used for directory layout and socket paths.
pub const APP_NAME: &str = "handfast";

pub mod bus;
pub mod error;
pub mod paths;
pub mod store;
pub mod supervise;
