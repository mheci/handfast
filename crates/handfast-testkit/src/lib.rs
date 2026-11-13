//! Independent Rust peer and golden-vector harness for KDE Connect.
//!
//! This crate intentionally does not depend on `handfast-proto` in its normal
//! build. Conformance tests live in `tests/` and use the production codec only
//! as a *consumer* of the frames produced here.

#![deny(missing_docs)]

pub mod frame;
pub mod payload_peer;
pub mod peer;
pub mod tls_peer;
pub mod vectors;

pub use frame::WirePacket;
pub use payload_peer::{
    run_payload_self_test, PayloadReceiveReport, PayloadReceiver, PayloadReceiverConfig,
    PayloadSelfTestCase, PayloadSelfTestReport, PayloadSendReport, PayloadSender,
    PayloadSenderConfig,
};
pub use peer::{run_self_test, PeerConfig, SelfTestReport};
pub use tls_peer::{run_tls_self_test, TlsSelfTestReport};
