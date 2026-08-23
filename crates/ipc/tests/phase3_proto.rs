#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Phase 3 serialization round-trip coverage for the new [`Request`] and
//! [`ServerEvent`] variants.
//!
//! Pure serde checks over `serde_json`; no sockets required, so this file runs
//! on every platform (`integration.rs` covers transport, unix-only).

use serde_json::{json, Value};

use handfast_ipc::{Request, ServerEvent};

/// Round-trip `value` through JSON and return the intermediate representation
/// so callers can additionally assert on the wire shape.
fn roundtrip_request(request: &Request) -> Value {
    let raw = serde_json::to_value(request).expect("request serializes");
    let back: Request = serde_json::from_value(raw.clone()).expect("request deserializes");
    let re_raw = serde_json::to_value(&back).expect("re-serializes");
    assert_eq!(raw, re_raw, "round-trip is stable for {raw}");
    raw
}

/// Same as [`roundtrip_request`] for server events.
fn roundtrip_event(event: &ServerEvent) -> Value {
    let raw = serde_json::to_value(event).expect("event serializes");
    let back: ServerEvent = serde_json::from_value(raw.clone()).expect("event deserializes");
    let re_raw = serde_json::to_value(&back).expect("re-serializes");
    assert_eq!(raw, re_raw, "round-trip is stable for {raw}");
    raw
}

#[test]
fn transfer_requests_roundtrip_with_snake_case_tags() {
    let wire = roundtrip_request(&Request::TransferList);
    assert_eq!(wire, json!({ "method": "transfer_list" }));

    let wire = roundtrip_request(&Request::TransferCancel {
        transfer_id: "t-1".into(),
    });
    assert_eq!(
        wire,
        json!({ "method": "transfer_cancel", "params": { "transfer_id": "t-1" } })
    );
}

#[test]
fn command_requests_roundtrip() {
    let wire = roundtrip_request(&Request::RunCommandList {
        device_id: "d1".into(),
    });
    assert_eq!(
        wire,
        json!({ "method": "run_command_list", "params": { "device_id": "d1" } })
    );

    let wire = roundtrip_request(&Request::RunCommand {
        device_id: "d1".into(),
        command_name: "screenshot".into(),
    });
    assert_eq!(
        wire,
        json!({
            "method": "run_command",
            "params": { "device_id": "d1", "command_name": "screenshot" }
        })
    );
}

#[test]
fn volume_requests_roundtrip() {
    let wire = roundtrip_request(&Request::SetVolume { percent: 42 });
    assert_eq!(
        wire,
        json!({ "method": "set_volume", "params": { "percent": 42 } })
    );

    let wire = roundtrip_request(&Request::GetVolume);
    assert_eq!(wire, json!({ "method": "get_volume" }));
}

#[test]
fn share_and_battery_and_sms_requests_roundtrip() {
    let wire = roundtrip_request(&Request::ShareText {
        device_id: "d1".into(),
        text: "hello".into(),
    });
    assert_eq!(
        wire,
        json!({ "method": "share_text", "params": { "device_id": "d1", "text": "hello" } })
    );

    let wire = roundtrip_request(&Request::ShareUrl {
        device_id: "d1".into(),
        url: "https://example.com".into(),
    });
    assert_eq!(
        wire,
        json!({ "method": "share_url", "params": { "device_id": "d1", "url": "https://example.com" } })
    );

    let wire = roundtrip_request(&Request::RequestBattery {
        device_id: "d2".into(),
    });
    assert_eq!(
        wire,
        json!({ "method": "request_battery", "params": { "device_id": "d2" } })
    );

    let wire = roundtrip_request(&Request::SendSms {
        device_id: "d2".into(),
        number: "+15550001111".into(),
        text: "ping".into(),
    });
    assert_eq!(
        wire,
        json!({
            "method": "send_sms",
            "params": { "device_id": "d2", "number": "+15550001111", "text": "ping" }
        })
    );
}

#[test]
fn transfer_events_roundtrip() {
    let wire = roundtrip_event(&ServerEvent::TransferAdded {
        id: "t-9".into(),
        device_id: "d1".into(),
        direction: "incoming".into(),
        file_name: "report.pdf".into(),
        total: 2048,
    });
    assert_eq!(
        wire,
        json!({
            "event": "transfer_added",
            "data": {
                "id": "t-9",
                "device_id": "d1",
                "direction": "incoming",
                "file_name": "report.pdf",
                "total": 2048
            }
        })
    );

    let wire = roundtrip_event(&ServerEvent::TransferFinished { id: "t-9".into() });
    assert_eq!(
        wire,
        json!({ "event": "transfer_finished", "data": { "id": "t-9" } })
    );

    let wire = roundtrip_event(&ServerEvent::TransferFailed {
        id: "t-10".into(),
        reason: "peer disconnected".into(),
    });
    assert_eq!(
        wire,
        json!({ "event": "transfer_failed", "data": { "id": "t-10", "reason": "peer disconnected" } })
    );
}

#[test]
fn battery_telephony_volume_command_events_roundtrip() {
    let wire = roundtrip_event(&ServerEvent::BatteryChanged {
        device_id: "d1".into(),
        level: 87,
        charging: true,
    });
    assert_eq!(
        wire,
        json!({
            "event": "battery_changed",
            "data": { "device_id": "d1", "level": 87, "charging": true }
        })
    );

    let wire = roundtrip_event(&ServerEvent::TelephonyEvent {
        device_id: "d1".into(),
        state: "ringing".into(),
        number: Some("+15550002222".into()),
    });
    assert_eq!(
        wire,
        json!({
            "event": "telephony_event",
            "data": { "device_id": "d1", "state": "ringing", "number": "+15550002222" }
        })
    );

    let back: ServerEvent =
        serde_json::from_value(json!({ "event": "telephony_event", "data": { "device_id": "d1", "state": "idle", "number": null } }))
            .expect("telephony_event with null number deserializes");
    match back {
        ServerEvent::TelephonyEvent {
            device_id,
            state,
            number,
        } => {
            assert_eq!(device_id, "d1");
            assert_eq!(state, "idle");
            assert_eq!(number, None);
        }
        other => panic!("expected TelephonyEvent, got {other:?}"),
    }

    let wire = roundtrip_event(&ServerEvent::VolumeChanged {
        percent: 33,
        muted: false,
    });
    assert_eq!(
        wire,
        json!({ "event": "volume_changed", "data": { "percent": 33, "muted": false } })
    );

    let wire = roundtrip_event(&ServerEvent::CommandResult {
        device_id: "d3".into(),
        name: "battery".into(),
        success: true,
        output: "ok".into(),
    });
    assert_eq!(
        wire,
        json!({
            "event": "command_result",
            "data": { "device_id": "d3", "name": "battery", "success": true, "output": "ok" }
        })
    );
}
