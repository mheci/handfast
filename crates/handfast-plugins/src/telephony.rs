//! Show the phone's incoming calls on this desktop.
//!
//! Upstream's `telephony` plugin is a notification listener, not a modem: the
//! phone sends `kdeconnect.telephony` with `event`, `phoneNumber`,
//! `contactName` and sometimes a `phoneThumbnail`, and the desktop raises a
//! persistent notification the user can act on. `talking` closes the previous
//! notification instead of opening a new one.
//!
//! The outbound ModemManager direction lives in [`crate::mmtelephony`]. The two
//! are separate plugins upstream and stay separate here, because a desktop can
//! easily want one and not the other.
//!
//! `isCancel` is honoured the same way upstream is: a ringing event that is
//! cancelled must withdraw the notification, or a missed call leaves a stale
//! popup behind.

use async_trait::async_trait;
use handfast_desktop::Notification;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::Value;

use crate::roster;

/// Longest accepted phone number, in bytes.
const MAX_NUMBER_BYTES: usize = 64;

/// Longest accepted contact name, in bytes.
const MAX_NAME_BYTES: usize = 256;

/// Wire event names this plugin understands.
const EVENTS: [&str; 4] = ["ringing", "talking", "missedCall", "sms"];

/// One call event from the phone, as the desktop needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingCall {
    /// Wire event name.
    pub event: String,
    /// Caller identifier, when the phone sent one.
    pub phone_number: Option<String>,
    /// Resolved contact name.
    pub contact_name: Option<String>,
    /// Whether this cancels the previous event.
    pub is_cancel: bool,
}

impl IncomingCall {
    /// Read a telephony body, or `None` when the event is unusable.
    ///
    /// An unknown event is refused rather than shown: a notification whose
    /// text is a raw protocol string is worse than no notification.
    #[must_use]
    pub fn from_body(body: &serde_json::Map<String, Value>) -> Option<Self> {
        let event = body.get("event")?.as_str()?;
        if !EVENTS.contains(&event) {
            return None;
        }
        let phone_number = body
            .get("phoneNumber")
            .and_then(Value::as_str)
            .filter(|number| !number.is_empty() && number.len() <= MAX_NUMBER_BYTES)
            .map(str::to_string);
        let contact_name = body
            .get("contactName")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty() && name.len() <= MAX_NAME_BYTES)
            .map(str::to_string);
        Some(Self {
            event: event.to_string(),
            phone_number,
            contact_name,
            is_cancel: body
                .get("isCancel")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// Who is calling, for the notification body.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.contact_name
            .clone()
            .or_else(|| self.phone_number.clone())
            .unwrap_or_else(|| "unknown number".to_string())
    }

    /// The notification this event should raise, if any.
    #[must_use]
    pub fn notification(&self) -> Option<Notification> {
        if self.is_cancel {
            return None;
        }
        let (title, body) = match self.event.as_str() {
            "ringing" => (
                "Incoming call".to_string(),
                format!("Incoming call from {}", self.display_name()),
            ),
            "missedCall" => (
                "Missed call".to_string(),
                format!("Missed call from {}", self.display_name()),
            ),
            "sms" => (
                "New message".to_string(),
                format!("Message from {}", self.display_name()),
            ),
            // A call in progress needs no popup; upstream closes the ringing
            // one instead.
            _ => return None,
        };
        Some(Notification::new("KDE Connect", title, body))
    }
}

/// Shows the phone's calls as desktop notifications.
#[derive(Debug, Default)]
pub struct TelephonyPlugin {
    /// Calls handled since the last log line.
    handled: usize,
    /// Whether a ringing notification is currently showing.
    ringing: bool,
}

impl TelephonyPlugin {
    /// Create a telephony plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            handled: 0,
            ringing: false,
        }
    }

    /// Calls handled so far.
    #[must_use]
    pub const fn handled(&self) -> usize {
        self.handled
    }

    /// Whether a call is currently ringing on screen.
    #[must_use]
    pub const fn is_ringing(&self) -> bool {
        self.ringing
    }
}

#[async_trait]
impl Plugin for TelephonyPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::TELEPHONY
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        match packet.ptype.as_str() {
            types::TELEPHONY => {
                let Some(call) = IncomingCall::from_body(&packet.body) else {
                    return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                        level: LogLevel::Warn,
                        message: "peer sent an unusable telephony event".to_string(),
                    }));
                };
                // Any event ends the previous ring, whether it is a cancel, a
                // talk or a missed call.
                let was_ringing = std::mem::replace(&mut self.ringing, call.event == "ringing");
                self.handled += 1;

                let Some(notifier) = &ctx.desktop.notifier else {
                    return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                        level: LogLevel::Info,
                        message: format!(
                            "call event {} from {} (no notification backend)",
                            call.event,
                            call.display_name()
                        ),
                    }));
                };
                if let Some(notification) = call.notification() {
                    notifier.notify(&notification).await.map_err(|error| {
                        PluginError::Backend(format!("notification failed: {error}"))
                    })?;
                }
                Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message: if was_ringing && !self.ringing {
                        format!("call from {} ended", call.display_name())
                    } else {
                        format!("call event: {}", call.event)
                    },
                }))
            }
            // The phone asking the desktop to mute is a modem-side action this
            // plugin has no adapter for; `mmtelephony` answers it.
            types::TELEPHONY_REQUEST_MUTE => Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Info,
                message: "peer asked to mute; no mute backend on this host".to_string(),
            })),
            _ => Ok(PluginOutput::new()),
        }
    }
}

/// Builds [`TelephonyPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct TelephonyFactory;

impl PluginFactory for TelephonyFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::TELEPHONY
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(TelephonyPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginConfig, PluginStatus};

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Phone"),
            DeviceType::Phone,
        )
    }

    async fn send(
        plugin: &mut TelephonyPlugin,
        desktop: &handfast_desktop::Desktop,
        ptype: &str,
        body: Value,
    ) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
            config: PluginConfig::empty(),
        };
        let packet = Packet::from_body(ptype, &body).unwrap();
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    #[test]
    fn the_roster_entry_matches_upstream() {
        assert_eq!(roster::TELEPHONY.status, PluginStatus::Implemented);
        assert_eq!(roster::TELEPHONY.incoming, &[types::TELEPHONY]);
        assert_eq!(
            roster::TELEPHONY.outgoing,
            &[types::TELEPHONY_REQUEST_MUTE]
        );
    }

    #[tokio::test]
    async fn a_ringing_call_raises_a_notification() {
        let mock = MockDesktop::new();
        let mut plugin = TelephonyPlugin::new();
        let output = send(
            &mut plugin,
            &mock.desktop(),
            types::TELEPHONY,
            serde_json::json!({
                "event": "ringing",
                "phoneNumber": "5551234",
                "contactName": "Alice",
            }),
        )
        .await;

        assert_eq!(output.events.len(), 1);
        assert!(plugin.is_ringing());
        assert_eq!(plugin.handled(), 1);
        let notifications = mock.notifications();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].title, "Incoming call");
        assert!(notifications[0].text.contains("Alice"));
    }

    #[tokio::test]
    async fn talking_closes_the_ring_without_a_new_notification() {
        let mock = MockDesktop::new();
        let mut plugin = TelephonyPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            types::TELEPHONY,
            serde_json::json!({ "event": "ringing", "contactName": "Alice" }),
        )
        .await;
        send(
            &mut plugin,
            &mock.desktop(),
            types::TELEPHONY,
            serde_json::json!({ "event": "talking", "contactName": "Alice" }),
        )
        .await;

        // Only the ring was shown.
        assert_eq!(mock.notifications().len(), 1);
        assert!(!plugin.is_ringing());
        assert_eq!(plugin.handled(), 2);
    }

    #[tokio::test]
    async fn a_cancel_does_not_raise_a_notification() {
        let mock = MockDesktop::new();
        let mut plugin = TelephonyPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            types::TELEPHONY,
            serde_json::json!({ "event": "ringing", "isCancel": true }),
        )
        .await;
        assert!(mock.notifications().is_empty());
        assert!(!plugin.is_ringing());
    }

    #[tokio::test]
    async fn a_missed_call_is_reported() {
        let mock = MockDesktop::new();
        let mut plugin = TelephonyPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            types::TELEPHONY,
            serde_json::json!({ "event": "missedCall", "phoneNumber": "5559999" }),
        )
        .await;
        let notifications = mock.notifications();
        assert_eq!(notifications[0].title, "Missed call");
        assert!(notifications[0].text.contains("5559999"));
    }

    #[test]
    fn a_nameless_caller_shows_the_number_then_a_placeholder() {
        let body = serde_json::json!({ "event": "ringing", "phoneNumber": "5551234" });
        let call = IncomingCall::from_body(body.as_object().unwrap()).unwrap();
        assert_eq!(call.display_name(), "5551234");

        let anonymous = serde_json::json!({ "event": "ringing" });
        let call = IncomingCall::from_body(anonymous.as_object().unwrap()).unwrap();
        assert_eq!(call.display_name(), "unknown number");
    }

    #[test]
    fn unusable_events_are_refused() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "event": "nonsense" }),
            serde_json::json!({ "event": 7 }),
        ] {
            assert!(
                IncomingCall::from_body(body.as_object().unwrap()).is_none(),
                "body {body} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn an_unusable_event_is_logged_not_shown() {
        let mock = MockDesktop::new();
        let mut plugin = TelephonyPlugin::new();
        let output = send(
            &mut plugin,
            &mock.desktop(),
            types::TELEPHONY,
            serde_json::json!({ "event": "nonsense" }),
        )
        .await;
        assert!(mock.notifications().is_empty());
        assert_eq!(output.events.len(), 1);
    }

    #[tokio::test]
    async fn a_mute_request_is_answered_with_a_log() {
        let mock = MockDesktop::new();
        let mut plugin = TelephonyPlugin::new();
        let output = send(
            &mut plugin,
            &mock.desktop(),
            types::TELEPHONY_REQUEST_MUTE,
            serde_json::json!({}),
        )
        .await;
        assert_eq!(output.events.len(), 1);
    }
}
