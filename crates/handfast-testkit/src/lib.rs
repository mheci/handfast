//! Independent Rust peer and golden-vector harness for KDE Connect.
//!
//! This crate intentionally does not depend on `handfast-proto` in its normal
//! build. Conformance tests live in `tests/` and use the production codec only
//! as a *consumer* of the frames produced here.

#![deny(missing_docs)]

pub mod frame;
pub mod peer;
pub mod vectors;

pub use frame::WirePacket;
pub use peer::{run_self_test, PeerConfig, SelfTestReport};
