//! Admission policy for unpaired links and inbound connections.
//!
//! The policy is the only owner of the unpaired slot count and the inbound
//! per-IP window. The manager creates one policy and clones it into handshake
//! tasks. A short mutex keeps the cap exact across concurrent accepts; it does
//! not guard sockets or link state.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use handfast_proto::DeviceId;

use crate::error::{NetError, Result};
use crate::identity::PeerTrust;
use crate::link::LinkLimits;

#[derive(Debug)]
struct PolicyState {
    unpaired: usize,
    inbound: HashMap<IpAddr, Instant>,
}

/// The admission policy shared by the manager and its handshake tasks.
#[derive(Clone)]
pub struct LinkPolicy {
    state: Arc<Mutex<PolicyState>>,
    limits: LinkLimits,
}

impl LinkPolicy {
    /// Build a policy with the given limits.
    #[must_use]
    pub fn new(limits: LinkLimits) -> Self {
        Self {
            state: Arc::new(Mutex::new(PolicyState {
                unpaired: 0,
                inbound: HashMap::new(),
            })),
            limits,
        }
    }

    /// Reserve a slot for an incoming connection.
    ///
    /// Every inbound connection passes the per-IP window. An unpaired peer also
    /// consumes one of the unpaired slots; a pinned peer does not. The returned
    /// permit releases its slot on drop.
    ///
    /// # Errors
    ///
    /// Fails when the IP is inside the inbound window or every unpaired slot is
    /// reserved or active.
    pub fn reserve_incoming(
        &self,
        address: IpAddr,
        peer: &DeviceId,
        trust: PeerTrust,
    ) -> Result<AdmissionPermit> {
        let now = Instant::now();
        let mut state = self.lock();

        if let Some(last) = state.inbound.get(&address) {
            if now.duration_since(*last) < self.limits.per_ip_interval {
                return Err(NetError::Admission(format!(
                    "inbound connection from {address} inside the rate window"
                )));
            }
        }

        if state.inbound.len() >= self.limits.rate_limit_entries
            && !state.inbound.contains_key(&address)
        {
            if let Some(oldest) = state
                .inbound
                .iter()
                .min_by_key(|(_, instant)| **instant)
                .map(|(ip, _)| *ip)
            {
                state.inbound.remove(&oldest);
            }
        }
        state.inbound.insert(address, now);

        self.reserve_unpaired(state, peer, trust)
    }

    /// Reserve a slot for an outgoing dial.
    ///
    /// This reserves the unpaired slot but does not mark the inbound per-IP
    /// window, so the reciprocal accept of a simultaneously discovering peer
    /// is not rejected. Per-peer backoff handles retries.
    ///
    /// # Errors
    ///
    /// Fails when every unpaired slot is reserved or active.
    pub fn reserve_outgoing(&self, peer: &DeviceId, trust: PeerTrust) -> Result<AdmissionPermit> {
        let state = self.lock();
        self.reserve_unpaired(state, peer, trust)
    }

    fn reserve_unpaired(
        &self,
        mut state: MutexGuard<'_, PolicyState>,
        peer: &DeviceId,
        trust: PeerTrust,
    ) -> Result<AdmissionPermit> {
        let unpaired = matches!(trust, PeerTrust::Unpaired);
        if unpaired {
            if state.unpaired >= self.limits.max_unpaired_links {
                return Err(NetError::Admission(format!(
                    "unpaired link cap reached for {peer}"
                )));
            }
            state.unpaired += 1;
        }
        drop(state);
        Ok(AdmissionPermit {
            state: Arc::clone(&self.state),
            unpaired,
        })
    }

    fn lock(&self) -> MutexGuard<'_, PolicyState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A reserved unpaired slot. Dropping it releases the slot.
pub struct AdmissionPermit {
    state: Arc<Mutex<PolicyState>>,
    unpaired: bool,
}

impl AdmissionPermit {
    /// Whether this permit consumes an unpaired slot.
    #[must_use]
    pub(crate) fn is_unpaired(&self) -> bool {
        self.unpaired
    }
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        if self.unpaired {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.unpaired = state.unpaired.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use super::*;
    use crate::identity::Fingerprint;

    const PEER: &str = "0123456789abcdef0123456789abcdef";

    fn peer() -> DeviceId {
        DeviceId::new(PEER).expect("the test device ID is valid")
    }

    fn limits() -> LinkLimits {
        LinkLimits::default()
    }

    fn address(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, last))
    }

    #[test]
    fn reserve_incoming_rejects_the_43rd_unpaired_link() {
        let policy = LinkPolicy::new(LinkLimits {
            per_ip_interval: Duration::ZERO,
            ..limits()
        });
        let mut permits = Vec::new();
        for _ in 0..42 {
            permits.push(
                policy
                    .reserve_incoming(address(1), &peer(), PeerTrust::Unpaired)
                    .expect("the first 42 unpaired slots are admitted"),
            );
        }
        assert!(matches!(
            policy.reserve_incoming(address(1), &peer(), PeerTrust::Unpaired),
            Err(NetError::Admission(_))
        ));
    }

    #[test]
    fn reserve_outgoing_counts_against_the_unpaired_cap() {
        let policy = LinkPolicy::new(LinkLimits {
            max_unpaired_links: 1,
            ..limits()
        });
        let permit = policy
            .reserve_outgoing(&peer(), PeerTrust::Unpaired)
            .expect("the first outbound slot is admitted");
        assert!(matches!(
            policy.reserve_outgoing(&peer(), PeerTrust::Unpaired),
            Err(NetError::Admission(_))
        ));
        drop(permit);
        assert!(policy
            .reserve_outgoing(&peer(), PeerTrust::Unpaired)
            .is_ok());
    }

    #[test]
    fn paired_peer_does_not_consume_an_unpaired_slot() {
        let policy = LinkPolicy::new(LinkLimits {
            max_unpaired_links: 0,
            per_ip_interval: Duration::ZERO,
            ..limits()
        });
        let pin = Fingerprint::of_certificate(b"certificate");
        assert!(policy
            .reserve_incoming(address(1), &peer(), PeerTrust::Pinned(pin))
            .is_ok());
    }

    #[test]
    fn inbound_window_rejects_one_ip_inside_the_interval() {
        let policy = LinkPolicy::new(LinkLimits {
            per_ip_interval: Duration::from_millis(80),
            ..limits()
        });
        assert!(policy
            .reserve_incoming(address(1), &peer(), PeerTrust::Unpaired)
            .is_ok());
        assert!(matches!(
            policy.reserve_incoming(address(1), &peer(), PeerTrust::Unpaired),
            Err(NetError::Admission(_))
        ));
        std::thread::sleep(Duration::from_millis(100));
        assert!(policy
            .reserve_incoming(address(1), &peer(), PeerTrust::Unpaired)
            .is_ok());
    }

    #[test]
    fn outbound_reservation_does_not_mark_the_inbound_window() {
        let policy = LinkPolicy::new(LinkLimits {
            per_ip_interval: Duration::from_secs(60),
            ..limits()
        });
        let _outgoing = policy
            .reserve_outgoing(&peer(), PeerTrust::Unpaired)
            .expect("the outbound slot is admitted");
        assert!(
            policy
                .reserve_incoming(address(1), &peer(), PeerTrust::Unpaired)
                .is_ok(),
            "the reciprocal accept is not rate limited by our own dial"
        );
    }

    #[test]
    fn rate_limit_table_evicts_the_oldest_entry() {
        let policy = LinkPolicy::new(LinkLimits {
            per_ip_interval: Duration::from_secs(60),
            rate_limit_entries: 2,
            ..limits()
        });
        assert!(policy
            .reserve_incoming(address(1), &peer(), PeerTrust::Unpaired)
            .is_ok());
        assert!(policy
            .reserve_incoming(address(2), &peer(), PeerTrust::Unpaired)
            .is_ok());
        assert!(policy
            .reserve_incoming(address(3), &peer(), PeerTrust::Unpaired)
            .is_ok());
        assert_eq!(policy.lock().inbound.len(), 2, "the table is capped");
        assert!(
            policy
                .reserve_incoming(address(1), &peer(), PeerTrust::Unpaired)
                .is_ok(),
            "the evicted address is no longer rate limited"
        );
    }
}
