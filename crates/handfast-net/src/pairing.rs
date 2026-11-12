//! Pairing state machine.
//!
//! The daemon owns the user-facing prompt. This type only tracks protocol
//! state and the 30-second request convention.

use handfast_proto::PairingRequest;

use crate::error::{NetError, Result};

/// Protocol pairing timeout in seconds.
pub const PAIRING_TIMEOUT_SECS: i64 = 30;

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
                if now_secs.saturating_sub(timestamp) > PAIRING_TIMEOUT_SECS
        )
    }

    /// Apply an inbound pairing packet.
    ///
    /// # Errors
    ///
    /// Fails when a peer accepts without a pending local request.
    pub fn handle(&mut self, request: &PairingRequest) -> Result<Option<PairingAction>> {
        if !request.pair {
            self.state = PairingState::Unpaired;
            return Ok(Some(PairingAction::Rejected));
        }
        if let Some(timestamp) = request.timestamp {
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
            .handle(&PairingRequest::request(200))
            .unwrap()
            .unwrap();
        assert_eq!(action, PairingAction::RequestReceived);
        session.accept().unwrap();
        assert_eq!(session.state(), PairingState::Paired);
    }

    #[test]
    fn reject_unpairs() {
        let mut session = PairingSession::new();
        session.request(1).unwrap();
        session.reject();
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
