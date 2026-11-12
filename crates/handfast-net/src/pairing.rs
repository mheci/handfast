//! Pairing state machine.
//!
//! The daemon owns the user-facing prompt. This type only tracks protocol
//! state and the 30-second request convention.

use std::time::{SystemTime, UNIX_EPOCH};

use handfast_proto::PairingRequest;

use crate::error::{NetError, Result};

/// Protocol pairing timeout in seconds.
pub const PAIRING_TIMEOUT_SECS: i64 = 30;

/// Allowed clock difference between peers, in seconds.
pub const MAX_CLOCK_SKEW_SECS: i64 = 300;

/// Pairing state for one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingState {
    /// Not paired.
    Unpaired,
    /// A request is waiting for a user decision.
    Requested {
        /// Request timestamp in seconds since the Unix epoch.
        timestamp: i64,
    },
    /// Pairing is complete.
    Paired,
}

/// What the caller should surface after an inbound pairing packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingAction {
    /// Show a pairing prompt.
    RequestReceived,
    /// The peer accepted our request.
    Accepted,
    /// The peer rejected or unpaired.
    Rejected,
}

/// Pure pairing state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingSession {
    state: PairingState,
}

impl Default for PairingSession {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingSession {
    /// Start unpaired.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: PairingState::Unpaired,
        }
    }

    /// Current state.
    #[must_use]
    pub const fn state(&self) -> PairingState {
        self.state
    }

    /// Start a pairing request from this side.
    ///
    /// # Errors
    ///
    /// Fails if already paired.
    pub fn request(&mut self, timestamp: i64) -> Result<PairingRequest> {
        if self.state == PairingState::Paired {
            return Err(NetError::Pairing("device is already paired".to_string()));
        }
        self.state = PairingState::Requested { timestamp };
        Ok(PairingRequest::request(timestamp))
    }

    /// Accept the pending request.
    ///
    /// # Errors
    ///
    /// Fails when there is no pending request.
    pub fn accept(&mut self) -> Result<PairingRequest> {
        if !matches!(self.state, PairingState::Requested { .. }) {
            return Err(NetError::Pairing(
                "no pairing request to accept".to_string(),
            ));
        }
        self.state = PairingState::Paired;
        Ok(PairingRequest::accept())
    }

    /// Reject the pending request or leave a paired state.
    #[must_use]
    pub fn reject(&mut self) -> PairingRequest {
        self.state = PairingState::Unpaired;
        PairingRequest::reject()
    }

    /// Whether the pending request has exceeded the 30-second convention.
    #[must_use]
    pub fn is_expired(&self, now_secs: i64) -> bool {
        matches!(
            self.state,
            PairingState::Requested { timestamp }
                if timestamp > now_secs.saturating_add(MAX_CLOCK_SKEW_SECS)
                    || now_secs.saturating_sub(timestamp) > PAIRING_TIMEOUT_SECS
        )
    }

    /// Apply an inbound pairing packet using the current clock.
    ///
    /// # Errors
    ///
    /// Fails when a peer accepts without a pending local request, or when a
    /// request timestamp is outside the allowed clock skew.
    pub fn handle(&mut self, request: &PairingRequest) -> Result<Option<PairingAction>> {
        self.handle_at(request, unix_now())
    }

    /// Apply an inbound pairing packet with an explicit clock.
    ///
    /// # Errors
    ///
    /// Fails when a peer accepts without a pending local request, or when a
    /// request timestamp is outside the allowed clock skew.
    pub fn handle_at(
        &mut self,
        request: &PairingRequest,
        now_secs: i64,
    ) -> Result<Option<PairingAction>> {
        if !request.pair {
            self.state = PairingState::Unpaired;
            return Ok(Some(PairingAction::Rejected));
        }
        if let Some(timestamp) = request.timestamp {
            if self.state == PairingState::Paired {
                return Ok(None);
            }
            if timestamp.saturating_sub(now_secs).saturating_abs() > MAX_CLOCK_SKEW_SECS {
                return Err(NetError::Pairing(
                    "pairing timestamp outside allowed clock skew".to_string(),
                ));
            }
            self.state = PairingState::Requested { timestamp };
            return Ok(Some(PairingAction::RequestReceived));
        }
        match self.state {
            PairingState::Requested { .. } => {
                self.state = PairingState::Paired;
                Ok(Some(PairingAction::Accepted))
            }
            PairingState::Paired => Ok(None),
            PairingState::Unpaired => Err(NetError::Pairing(
                "peer accepted pairing without a local request".to_string(),
            )),
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_request_then_accept() {
        let mut session = PairingSession::new();
        let request = session.request(100).unwrap();
        assert!(request.is_request());
        assert_eq!(session.state(), PairingState::Requested { timestamp: 100 });
        session.handle(&PairingRequest::accept()).unwrap();
        assert_eq!(session.state(), PairingState::Paired);
    }

    #[test]
    fn inbound_request_then_local_accept() {
        let mut session = PairingSession::new();
        let action = session
            .handle_at(&PairingRequest::request(200), 200)
            .unwrap()
            .unwrap();
        assert_eq!(action, PairingAction::RequestReceived);
        session.accept().unwrap();
        assert_eq!(session.state(), PairingState::Paired);
    }

    #[test]
    fn future_timestamp_is_rejected() {
        let mut session = PairingSession::new();
        assert!(session
            .handle_at(&PairingRequest::request(i64::MAX), 1_000)
            .is_err());
        assert_eq!(session.state(), PairingState::Unpaired);
    }

    #[test]
    fn paired_peer_cannot_reset_to_requested() {
        let mut session = PairingSession::new();
        session.request(1_000).unwrap();
        session.handle_at(&PairingRequest::accept(), 1_000).unwrap();
        assert_eq!(session.state(), PairingState::Paired);

        let action = session
            .handle_at(&PairingRequest::request(1_001), 1_001)
            .unwrap();
        assert_eq!(action, None);
        assert_eq!(session.state(), PairingState::Paired);
    }

    #[test]
    fn reject_unpairs() {
        let mut session = PairingSession::new();
        session.request(1).unwrap();
        let _ = session.reject();
        assert_eq!(session.state(), PairingState::Unpaired);
        assert_eq!(
            session.handle(&PairingRequest::reject()).unwrap(),
            Some(PairingAction::Rejected)
        );
    }

    #[test]
    fn unsolicited_accept_is_rejected() {
        let mut session = PairingSession::new();
        assert!(session.handle(&PairingRequest::accept()).is_err());
    }

    #[test]
    fn request_expires_after_thirty_seconds() {
        let mut session = PairingSession::new();
        session.request(1_000).unwrap();
        assert!(!session.is_expired(1_030));
        assert!(session.is_expired(1_031));
    }

    #[test]
    fn pairing_while_paired_is_rejected() {
        let mut session = PairingSession::new();
        session.request(1).unwrap();
        session.accept().unwrap();
        assert!(session.request(2).is_err());
    }
}
