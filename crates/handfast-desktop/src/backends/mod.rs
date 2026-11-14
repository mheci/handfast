//! Linux desktop backends.
//!
//! Each backend is independent, so a host missing one capability still gets the
//! others.

pub mod notifications;

use std::sync::Arc;

use crate::traits::Notifier;

/// Build the host notification backend.
#[must_use]
pub fn notifier() -> Arc<dyn Notifier> {
    Arc::new(notifications::CommandNotifier::new())
}
