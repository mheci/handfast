//! Report this host's calls to the paired phone.
//!
//! On a mobile build this is the important direction: the phone's own telephony
//! plugin is what rings the desktop. Here the host is a desktop, so the flow is
//! reversed — a desktop with a modem forwards its calls to the phone, and the
//! phone can mute the ringtone.
//!
//! Two packet families, matching upstream:
//!
//! - `kdeconnect.telephony` out, with `event` (`ringing`, `talking`,
//!   `missedCall`, `sms`), `phoneNumber`, `contactName` and `isCancel`.
//! - `kdeconnect.telephony.request_mute` in, which silences the current call.
//!
//! The mute request needs a [`handfast_desktop::Telephony`] adapter method that
//! a poll-only backend cannot provide, so muting is reported as unsupported
//! rather than silently ignored. That is honest: the phone's UI shows a mute
//! button, and a button that does nothing is worse than one that says why.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory,
    PluginMeta, PluginOutput,
};
use serde_json::{json, Map, Value};

use crate::roster;

/// Longest accepted phone number, in bytes.
const MAX_NUMBER_BYTES: usize = 64;

/// Longest accepted contact name, in bytes.
const MAX_NAME_BYTES: usize = 256;

/// One event to send to the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelephonyEvent {
    /// Wire event name: `ringing`, `talking`, `missedCall` or `sms`.
    pub event: String,
    /// Caller identifier.
    pub phone_number: Option<String>,
    /// Resolved contact name.
    pub contact_name: Option<String>,
    /// Whether this cancels the previously reported event.
    pub is_cancel: bool,
}

/// Wire event names Handfast will forward.
const EVENTS: [&str; 4] = ["ringing", "talking", "missedCall", "sms"];

impl TelephonyEvent {
    /// Validate the event and that its fields fit the wire limits.
    ///
    /// # Errors
    ///
    /// Fails for an unknown event name or an oversized field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !EVENTS.contains(&self.event.as_str()) {
            return Err("unknown telephony event");
        }
        if self
            .phone_number
            .as_ref()
            .is_some_and(|number| number.len() > MAX_NUMBER_BYTES)
        {
            return Err("phone number is too long");
        }
        if self
            .contact_name
            .as_ref()
            .is_some_and(|name| name.len() > MAX_NAME_BYTES)
        {
            return Err("contact name is too long");
        }
        Ok(())
    }

    fn to_body(&self) -> Value {
        let mut body = Map::new();
        body.insert("event".to_string(), json!(self.event));
        if let Some(number) = &self.phone_number {
            body.insert("phoneNumber".to_string(), json!(number));
        }
        if let Some(name) = &self.contact_name {
            body.insert("contactName".to_string(), json!(name));
        }
        if self.is_cancel {
            body.insert("isCancel".to_string(), json!(true));
        }
        Value::Object(body)
    }
}

/// Forwards host telephony events to the phone.
#[derive(Debug, Default)]
pub struct TelephonyPlugin {
    /// Events queued for the next tick.
    pending: Vec<TelephonyEvent>,
    /// Whether the mute button should be offered.
    mute_available: bool,
    /// Telephony adapter is present.
    available: bool,
}

impl TelephonyPlugin {
    /// Create a telephony plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
            mute_available: false,
            available: false,
        }
    }

    /// Queue an event for the peer.
    ///
    /// # Errors
    ///
    /// Fails when the event is not one of the four wire events, or a field is
    /// oversized.
    pub fn report(&mut self, event: TelephonyEvent) -> Result<(), &'static str> {
        event.validate()?;
        self.pending.push(event);
        Ok(())
    }

    /// Number of queued events.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Events queued so far, for a desktop UI.
    #[must_use]
    pub fn events(&self) -> &[TelephonyEvent] {
        &self.pending
    }
}

#[async_trait]
impl Plugin for TelephonyPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::TELEPHONY
    }


    async fn on_tick(&mut self, ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        let mut output = PluginOutput::new();

        // Pull anything the host modem reported since the last tick. A host
        // without a modem simply never has events, which is why the plugin is
        // requirement-gated rather than erroring here.
        if let Some(telephony) = &ctx.desktop.telephony {
            self.available = true;
            match telephony.poll().await {
                Ok(events) => {
                    for event in events {
                        let reported = TelephonyEvent {
                            event: event.event,
                            phone_number: event.phone_number,
                            contact_name: event.contact_name,
                            is_cancel: event.is_cancel,
                        };
                        // A backend is not trusted to produce a valid event;
                        // drop the bad ones rather than failing the tick.
                        if reported.validate().is_ok() {
                            self.pending.push(reported);
                        }
                    }
                }
                Err(error) => {
                    output = output.with_event(PluginEvent::Log {
                        level: LogLevel::Warn,
                        message: format!("telephony poll failed: {error}"),
                    });
                }
            }
        }

        for event in std::mem::take(&mut self.pending) {
            output = output.with_packet(
                Packet::new(types::TELEPHONY, event.to_body())
                    .map_err(|error| PluginError::InvalidPacket(error.to_string()))?,
            );
        }
        Ok(output)
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        match packet.ptype.as_str() {
            // The phone asks us to silence the ringer.
            types::TELEPHONY_REQUEST_MUTE => Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: if self.mute_available {
                    "peer asked to mute the call; this backend cannot mute".to_string()
                } else {
                    "peer asked to mute the call but no telephony backend is active".to_string()
                },
            })),
            // The phone reports its own call state. Handfast is the desktop
            // side, so this is informational only.
            types::TELEPHONY => Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Info,
                message: format!(
                    "peer telephony event: {}",
                    packet
                        .body
                        .get("event")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ),
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
    use handfast_desktop::{CallEvent, MockDesktop};
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginConfig, PluginStatus};

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Phone"),
            DeviceType::Phone,
        )
    }

    fn ringing() -> TelephonyEvent {
        TelephonyEvent {
            event: "ringing".to_string(),
            phone_number: Some("5551234".to_string()),
            contact_name: Some("Alice".to_string()),
            is_cancel: false,
        }
    }

    async fn tick(
        plugin: &mut TelephonyPlugin,
        desktop: &handfast_desktop::Desktop,
    ) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
            config: PluginConfig::empty(),
        };
        plugin.on_tick(&mut ctx).await.unwrap()
    }

    #[test]
    fn the_roster_entry_matches_upstream() {
        assert_eq!(roster::TELEPHONY.status, PluginStatus::Implemented);
        assert_eq!(
            roster::TELEPHONY.incoming,
            &[types::TELEPHONY, types::TELEPHONY_REQUEST_MUTE]
        );
        assert_eq!(roster::TELEPHONY.outgoing, &[types::TELEPHONY]);
    }

    #[tokio::test]
    async fn a_queued_event_is_sent_with_the_upstream_fields() {
        let mock = MockDesktop::new();
        let mut plugin = TelephonyPlugin::new();
        plugin.report(ringing()).unwrap();

        let output = tick(&mut plugin, &mock.desktop()).await;
        assert_eq!(output.packets.len(), 1);
        let body = &output.packets[0].body;
        assert_eq!(output.packets[0].ptype, types::TELEPHONY);
        assert_eq!(body.get("event"), Some(&json!("ringing")));
        assert_eq!(body.get("phoneNumber"), Some(&json!("5551234")));
        assert_eq!(body.get("contactName"), Some(&json!("Alice")));
        // Upstream omits isCancel when it is false.
        assert!(!body.contains_key("isCancel"));
    }

    #[tokio::test]
    async fn a_backend_event_is_forwarded() {
        let mock = MockDesktop::new();
        mock.push_call(CallEvent {
            event: "talking".to_string(),
            phone_number: Some("5559999".to_string()),
            contact_name: None,
            is_cancel: false,
        });
        let mut plugin = TelephonyPlugin::new();

        let output = tick(&mut plugin, &mock.desktop()).await;
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].body.get("event"), Some(&json!("talking")));
    }

    #[tokio::test]
    async fn polling_consumes_the_backend_queue() {
        let mock = MockDesktop::new();
        mock.push_call(CallEvent {
            event: "ringing".to_string(),
            phone_number: None,
            contact_name: None,
            is_cancel: false,
        });
        let mut plugin = TelephonyPlugin::new();
        assert_eq!(tick(&mut plugin, &mock.desktop()).await.packets.len(), 1);
        // Nothing new reported, nothing sent.
        assert!(tick(&mut plugin, &mock.desktop()).await.packets.is_empty());
    }

    #[tokio::test]
    async fn a_backend_event_with_a_bad_name_is_dropped() {
        let mock = MockDesktop::new();
        mock.push_call(CallEvent {
            event: "nonsense".to_string(),
            phone_number: None,
            contact_name: None,
            is_cancel: false,
        });
        let mut plugin = TelephonyPlugin::new();
        assert!(tick(&mut plugin, &mock.desktop()).await.packets.is_empty());
    }

    #[test]
    fn unknown_events_and_oversized_fields_are_refused() {
        let mut plugin = TelephonyPlugin::new();
        let mut event = ringing();
        event.event = "nonsense".to_string();
        assert!(plugin.report(event).is_err());

        let mut event = ringing();
        event.phone_number = Some("1".repeat(MAX_NUMBER_BYTES + 1));
        assert!(plugin.report(event).is_err());

        let mut event = ringing();
        event.contact_name = Some("n".repeat(MAX_NAME_BYTES + 1));
        assert!(plugin.report(event).is_err());
        assert_eq!(plugin.pending(), 0);
    }

    #[tokio::test]
    async fn a_cancel_event_sets_the_flag() {
        let mock = MockDesktop::new();
        let mut plugin = TelephonyPlugin::new();
        let mut event = ringing();
        event.is_cancel = true;
        plugin.report(event).unwrap();
        let output = tick(&mut plugin, &mock.desktop()).await;
        assert_eq!(output.packets[0].body.get("isCancel"), Some(&json!(true)));
    }

    #[tokio::test]
    async fn a_mute_request_is_reported_not_ignored() {
        let mock = MockDesktop::new();
        let mut plugin = TelephonyPlugin::new();
        tick(&mut plugin, &mock.desktop()).await;

        let device = device();
        let desktop = mock.desktop();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let packet = Packet::from_body(types::TELEPHONY_REQUEST_MUTE, &json!({})).unwrap();
        let output = plugin.on_packet(&mut ctx, &packet).await.unwrap();
        assert_eq!(output.events.len(), 1);
    }
}
