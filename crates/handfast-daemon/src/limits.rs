//! Share limits, the single owner of the file, payload and queue bounds.
//!
//! [`ShareLimits::files`] and [`ShareLimits::payloads`] derive both adapters
//! from one set of numbers, so the declared file cap and the payload read cap
//! cannot disagree.

use std::time::Duration;

use handfast_desktop::FileLimits;
use handfast_net::PayloadLimits;

/// Age after which the startup sweep removes a stale `.part` file.
pub const STALE_TEMP_AGE: Duration = Duration::from_secs(24 * 3600);

/// File, payload and queue limits for the share path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareLimits {
    /// Largest declared incoming or outgoing file in bytes.
    pub max_file_size: u64,
    /// Extra bytes accepted beyond the declared size.
    pub max_extra: u64,
    /// Free space kept unused on the incoming filesystem.
    pub min_free_space: u64,
    /// Largest file name component in bytes.
    pub max_name_bytes: usize,
    /// Bytes reserved for a duplicate suffix such as ` (99)`.
    pub suffix_reserve: usize,
    /// Receives one peer may queue behind its active transfer.
    pub max_queued_receives: usize,
    /// Slots in the manager's reliable share channel.
    pub share_event_queue: usize,
    /// How long a share packet waits for room in the session queue.
    pub share_delivery_timeout: Duration,
}

impl Default for ShareLimits {
    fn default() -> Self {
        Self {
            max_file_size: 2 * 1024 * 1024 * 1024,
            max_extra: 64 * 1024 * 1024,
            min_free_space: 64 * 1024 * 1024,
            max_name_bytes: 255,
            suffix_reserve: 16,
            max_queued_receives: 8,
            share_event_queue: handfast_net::SHARE_EVENT_QUEUE,
            share_delivery_timeout: Duration::from_secs(2),
        }
    }
}

impl ShareLimits {
    /// Limits for the desktop files adapter.
    #[must_use]
    pub fn files(&self) -> FileLimits {
        FileLimits {
            max_file_size: self.max_file_size,
            max_extra: self.max_extra,
            min_free_space: self.min_free_space,
            max_name_bytes: self.max_name_bytes,
            suffix_reserve: self.suffix_reserve,
        }
    }

    /// Limits for the payload service.
    ///
    /// Timeouts, ports and pool sizes keep their designed defaults; the size
    /// caps come from this owner so the write reservation and the read cap
    /// agree.
    #[must_use]
    pub fn payloads(&self) -> PayloadLimits {
        PayloadLimits {
            max_size: self.max_file_size,
            max_extra: self.max_extra,
            ..PayloadLimits::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use handfast_proto::{PAYLOAD_PORT_MAX, PAYLOAD_PORT_MIN};

    #[test]
    fn files_and_payloads_share_one_size_owner() {
        let limits = ShareLimits::default();
        let files = limits.files();
        let payloads = limits.payloads();

        assert_eq!(files.max_file_size, payloads.max_size);
        assert_eq!(files.max_extra, payloads.max_extra);
        assert_eq!(files.max_file_size, 2 * 1024 * 1024 * 1024);
        assert_eq!(files.max_extra, 64 * 1024 * 1024);
        assert_eq!(files.min_free_space, 64 * 1024 * 1024);
        assert_eq!(files.max_name_bytes, 255);
        assert_eq!(files.suffix_reserve, 16);
    }

    #[test]
    fn payload_defaults_stay_inside_the_android_timeout() {
        let payloads = ShareLimits::default().payloads();

        assert_eq!(payloads.connect_timeout, Duration::from_secs(5));
        assert_eq!(payloads.handshake_timeout, Duration::from_secs(8));
        assert_eq!(payloads.accept_timeout, Duration::from_secs(30));
        assert_eq!(payloads.io_timeout, Duration::from_secs(8));
        assert_eq!(payloads.max_attempts, 3);
        assert_eq!(payloads.buffer_size, 64 * 1024);
        assert_eq!(payloads.max_active, 8);
        assert_eq!(payloads.max_pending_connects, 8);
        assert_eq!(payloads.port_min, PAYLOAD_PORT_MIN);
        assert_eq!(payloads.port_max, PAYLOAD_PORT_MAX);
        assert_eq!(payloads.port_min, 1739);
        assert_eq!(payloads.port_max, 1764);
    }

    #[test]
    fn queue_limits_match_the_design() {
        let limits = ShareLimits::default();

        assert_eq!(limits.max_queued_receives, 8);
        assert_eq!(limits.share_event_queue, 32);
        assert_eq!(limits.share_delivery_timeout, Duration::from_secs(2));
        assert_eq!(STALE_TEMP_AGE, Duration::from_secs(24 * 3600));
    }

    #[test]
    fn a_custom_size_flows_into_both_adapters() {
        let limits = ShareLimits {
            max_file_size: 4096,
            max_extra: 512,
            ..ShareLimits::default()
        };

        assert_eq!(limits.files().max_file_size, 4096);
        assert_eq!(limits.files().max_extra, 512);
        assert_eq!(limits.payloads().max_size, 4096);
        assert_eq!(limits.payloads().max_extra, 512);
    }
}
