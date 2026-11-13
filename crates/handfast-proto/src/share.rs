//! KDE Connect share request bodies and payload offers.
//!
//! A `kdeconnect.share.request` carries exactly one of a file, text or URL. A
//! file may announce a payload through the top-level `payloadSize` and
//! `payloadTransferInfo.port` fields. The protocol writes `-1` for an unknown
//! stream length and omits both payload fields for an empty file.
//!
//! `kdeconnect.share.request.update` carries batch progress metadata only.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::error::ProtoError;
use crate::packet::Packet;
use crate::types;

/// Lowest payload TCP port used by the official clients.
pub const PAYLOAD_PORT_MIN: u16 = 1739;
/// Highest payload TCP port used by the official clients.
pub const PAYLOAD_PORT_MAX: u16 = 1764;

/// Declared payload size.
///
/// Serializes as a JSON number: a known count as itself and [`Self::Unknown`]
/// as the protocol's `-1` stream marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadSize {
    /// A known number of payload bytes.
    Known(u64),
    /// Unknown length, written as `-1`. The receiver reads until EOF.
    Unknown,
}

impl Serialize for PayloadSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Known(size) => serializer.serialize_u64(*size),
            Self::Unknown => serializer.serialize_i64(-1),
        }
    }
}

impl<'de> Deserialize<'de> for PayloadSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadSizeVisitor;

        impl<'de> Visitor<'de> for PayloadSizeVisitor {
            type Value = PayloadSize;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a payload size of -1 or a non-negative integer")
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    -1 => Ok(PayloadSize::Unknown),
                    value if value >= 0 => Ok(PayloadSize::Known(value as u64)),
                    value => Err(E::custom(format!("invalid payload size {value}"))),
                }
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(PayloadSize::Known(value))
            }
        }

        deserializer.deserialize_any(PayloadSizeVisitor)
    }
}

/// A validated payload size and port pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadOffer {
    /// Declared payload size.
    pub size: PayloadSize,
    /// Payload TCP port, inside [`PAYLOAD_PORT_MIN`]-[`PAYLOAD_PORT_MAX`].
    pub port: u16,
}

/// Batch metadata shared by file requests and update packets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareBatch {
    /// Number of files in the batch. At least 1.
    pub number_of_files: u32,
    /// Sum of the declared payload sizes. May be 0.
    pub total_payload_size: u64,
}

/// A file share request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareFile {
    /// Original filename. The receiver falls back to a generated name when it
    /// is absent.
    pub filename: Option<String>,
    /// Payload size. [`PayloadSize::Known`]`(0)` is an empty file with no
    /// payload connection.
    pub size: PayloadSize,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_ms: Option<i64>,
    /// Last modification time in milliseconds since the Unix epoch.
    pub modified_ms: Option<i64>,
    /// Whether the receiver should open the file after saving it.
    pub open: bool,
    /// Batch metadata, present only for multi-file shares.
    pub batch: Option<ShareBatch>,
}

/// A decoded share request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareRequest {
    /// A file, possibly with a payload.
    File(ShareFile),
    /// Clipboard text.
    Text(String),
    /// A URL. The consumer decides which schemes are safe to open.
    Url(String),
}

/// Errors produced by the share parsers.
#[derive(Debug, Error)]
pub enum ShareError {
    /// The packet type is not the expected share packet.
    #[error("not a share request")]
    NotShareRequest,
    /// The body has none of `filename`, `text` or `url`.
    #[error("share request has no file, text or url")]
    NothingAttached,
    /// The body mixes file, text or URL fields.
    #[error("share request has more than one of file, text or url")]
    MultipleAttachments,
    /// A payload size was below `-1`.
    #[error("invalid payload size {0}")]
    InvalidSize(i64),
    /// A payload port was outside 1739-1764.
    #[error("invalid payload port {0}")]
    InvalidPort(u16),
    /// Batch metadata was incomplete or out of range.
    #[error("invalid batch metadata")]
    InvalidBatch,
    /// A body field had the wrong JSON type.
    #[error("invalid share field {0}")]
    InvalidField(&'static str),
    /// The packet layer rejected the body.
    #[error("protocol: {0}")]
    Proto(#[from] ProtoError),
}

impl ShareRequest {
    /// Parse a `kdeconnect.share.request`.
    ///
    /// A file body is recognized by `filename`, by file metadata fields, or by
    /// a top-level payload declaration. Text and URL bodies ignore payload
    /// fields.
    ///
    /// # Errors
    ///
    /// Fails for another packet type, a body with no file, text or URL, a body
    /// that mixes them, a negative size other than -1, a non-zero size without
    /// a port, or a payload port outside 1739-1764.
    pub fn from_packet(packet: &Packet) -> Result<Self, ShareError> {
        if packet.ptype != types::SHARE_REQUEST {
            return Err(ShareError::NotShareRequest);
        }
        check_payload(packet)?;

        let has_file = packet.body.contains_key("filename")
            || packet.body.contains_key("creationTime")
            || packet.body.contains_key("lastModified")
            || packet.body.contains_key("open")
            || packet.body.contains_key("numberOfFiles")
            || packet.body.contains_key("totalPayloadSize");
        let has_text = packet.body.contains_key("text");
        let has_url = packet.body.contains_key("url");

        match (has_file, has_text, has_url) {
            (false, false, false) if packet.payload_size.is_some() => {
                Ok(Self::File(parse_file(packet)?))
            }
            (false, false, false) => Err(ShareError::NothingAttached),
            (true, false, false) => Ok(Self::File(parse_file(packet)?)),
            (false, true, false) => Ok(Self::Text(required_string(&packet.body, "text")?)),
            (false, false, true) => Ok(Self::Url(required_string(&packet.body, "url")?)),
            _ => Err(ShareError::MultipleAttachments),
        }
    }

    /// Parse a `kdeconnect.share.request.update`.
    ///
    /// # Errors
    ///
    /// Fails for another packet type, missing or malformed batch fields, or
    /// `numberOfFiles` below 1.
    pub fn update_from_packet(packet: &Packet) -> Result<ShareBatch, ShareError> {
        if packet.ptype != types::SHARE_REQUEST_UPDATE {
            return Err(ShareError::NotShareRequest);
        }
        check_payload(packet)?;
        required_batch(&packet.body)
    }
}

/// Build a file share request.
///
/// A zero-size file omits both payload fields and ignores `port`, matching the
/// official clients. A non-empty file requires a port inside
/// [`PAYLOAD_PORT_MIN`]-[`PAYLOAD_PORT_MAX`]. Batch fields are emitted only
/// when a batch is present.
///
/// A file with no filename, no metadata, no batch and a zero size has no wire
/// representation; official clients key file bodies on `filename`.
///
/// # Errors
///
/// Fails when a non-empty file has no port, the port is outside 1739-1764, the
/// size exceeds `i64::MAX`, or the batch has zero files.
pub fn file_request(file: &ShareFile, port: Option<u16>) -> Result<Packet, ProtoError> {
    let mut body = Map::new();
    if let Some(filename) = &file.filename {
        body.insert("filename".to_string(), json!(filename.as_str()));
    }
    if let Some(created) = file.created_ms {
        body.insert("creationTime".to_string(), json!(created));
    }
    if let Some(modified) = file.modified_ms {
        body.insert("lastModified".to_string(), json!(modified));
    }
    if file.open {
        body.insert("open".to_string(), json!(true));
    }
    if let Some(batch) = &file.batch {
        check_batch(batch)?;
        body.insert("numberOfFiles".to_string(), json!(batch.number_of_files));
        body.insert(
            "totalPayloadSize".to_string(),
            json!(batch.total_payload_size),
        );
    }

    let mut packet = Packet::new(types::SHARE_REQUEST, Value::Object(body))?;
    match file.size {
        PayloadSize::Known(0) => {}
        PayloadSize::Known(size) => {
            let port = required_port(port)?;
            let size = i64::try_from(size).map_err(|_| {
                ProtoError::InvalidPayload(format!("payload size {size} exceeds i64"))
            })?;
            packet = packet.with_payload(size, port)?;
        }
        PayloadSize::Unknown => {
            packet = packet.with_payload(-1, required_port(port)?)?;
        }
    }
    Ok(packet)
}

/// Build a text share request.
///
/// # Errors
///
/// Fails only if the packet body cannot be constructed.
pub fn text_request(text: &str) -> Result<Packet, ProtoError> {
    Packet::new(types::SHARE_REQUEST, json!({ "text": text }))
}

/// Build a URL share request.
///
/// # Errors
///
/// Fails only if the packet body cannot be constructed.
pub fn url_request(url: &str) -> Result<Packet, ProtoError> {
    Packet::new(types::SHARE_REQUEST, json!({ "url": url }))
}

/// Build a `kdeconnect.share.request.update` packet.
///
/// # Errors
///
/// Fails when the batch has zero files or the packet body cannot be
/// constructed.
pub fn update_request(batch: ShareBatch) -> Result<Packet, ProtoError> {
    check_batch(&batch)?;
    Packet::new(
        types::SHARE_REQUEST_UPDATE,
        json!({
            "numberOfFiles": batch.number_of_files,
            "totalPayloadSize": batch.total_payload_size,
        }),
    )
}

fn parse_file(packet: &Packet) -> Result<ShareFile, ShareError> {
    let size = match packet.payload_size {
        None => PayloadSize::Known(0),
        Some(-1) => PayloadSize::Unknown,
        Some(size) => PayloadSize::Known(size as u64),
    };
    Ok(ShareFile {
        filename: optional_string(&packet.body, "filename")?,
        size,
        created_ms: optional_i64(&packet.body, "creationTime")?,
        modified_ms: optional_i64(&packet.body, "lastModified")?,
        open: optional_bool(&packet.body, "open")?,
        batch: optional_batch(&packet.body)?,
    })
}

fn check_payload(packet: &Packet) -> Result<(), ShareError> {
    if let Some(size) = packet.payload_size.filter(|size| *size < -1) {
        return Err(ShareError::InvalidSize(size));
    }
    if packet.payload_size.is_some() != packet.payload_transfer_info.is_some() {
        return Err(ShareError::Proto(ProtoError::InvalidPayload(
            "payloadSize and payloadTransferInfo must appear together".to_string(),
        )));
    }
    let port = packet
        .payload_transfer_info
        .as_ref()
        .map(|info| info.port)
        .filter(|port| !(PAYLOAD_PORT_MIN..=PAYLOAD_PORT_MAX).contains(port));
    if let Some(port) = port {
        return Err(ShareError::InvalidPort(port));
    }
    Ok(())
}

fn required_string(body: &Map<String, Value>, key: &'static str) -> Result<String, ShareError> {
    match body.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(ShareError::InvalidField(key)),
    }
}

fn optional_string(
    body: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<String>, ShareError> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ShareError::InvalidField(key)),
    }
}

fn optional_i64(body: &Map<String, Value>, key: &'static str) -> Result<Option<i64>, ShareError> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .map(Some)
            .ok_or(ShareError::InvalidField(key)),
        Some(_) => Err(ShareError::InvalidField(key)),
    }
}

fn optional_bool(body: &Map<String, Value>, key: &'static str) -> Result<bool, ShareError> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ShareError::InvalidField(key)),
    }
}

fn optional_batch(body: &Map<String, Value>) -> Result<Option<ShareBatch>, ShareError> {
    if body.contains_key("numberOfFiles") || body.contains_key("totalPayloadSize") {
        Ok(Some(required_batch(body)?))
    } else {
        Ok(None)
    }
}

fn required_batch(body: &Map<String, Value>) -> Result<ShareBatch, ShareError> {
    let number_of_files = body
        .get("numberOfFiles")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value >= 1)
        .ok_or(ShareError::InvalidBatch)?;
    let total_payload_size = body
        .get("totalPayloadSize")
        .and_then(Value::as_u64)
        .ok_or(ShareError::InvalidBatch)?;
    Ok(ShareBatch {
        number_of_files,
        total_payload_size,
    })
}

fn check_batch(batch: &ShareBatch) -> Result<(), ProtoError> {
    if batch.number_of_files == 0 {
        return Err(ProtoError::InvalidPayload(
            "numberOfFiles must be at least 1".to_string(),
        ));
    }
    Ok(())
}

fn required_port(port: Option<u16>) -> Result<u16, ProtoError> {
    let port = port.ok_or_else(|| {
        ProtoError::InvalidPayload("a non-empty file needs a payload port".to_string())
    })?;
    if !(PAYLOAD_PORT_MIN..=PAYLOAD_PORT_MAX).contains(&port) {
        return Err(ProtoError::InvalidPayload(format!(
            "invalid payload port {port}"
        )));
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::PayloadTransferInfo;

    fn file() -> ShareFile {
        ShareFile {
            filename: Some("report.pdf".to_string()),
            size: PayloadSize::Known(882),
            created_ms: Some(1_700_000_000_000),
            modified_ms: Some(1_700_000_100_000),
            open: true,
            batch: Some(ShareBatch {
                number_of_files: 2,
                total_payload_size: 2048,
            }),
        }
    }

    fn file_packet() -> Packet {
        file_request(&file(), Some(1739)).unwrap()
    }

    fn empty_file() -> ShareFile {
        ShareFile {
            filename: Some("empty.txt".to_string()),
            size: PayloadSize::Known(0),
            created_ms: None,
            modified_ms: None,
            open: false,
            batch: None,
        }
    }

    #[test]
    fn payload_size_serializes_as_a_number() {
        assert_eq!(
            serde_json::to_value(PayloadSize::Known(882)).unwrap(),
            json!(882)
        );
        assert_eq!(
            serde_json::to_value(PayloadSize::Known(0)).unwrap(),
            json!(0)
        );
        assert_eq!(
            serde_json::to_value(PayloadSize::Unknown).unwrap(),
            json!(-1)
        );
    }

    #[test]
    fn payload_size_deserializes_from_a_number() {
        assert_eq!(
            serde_json::from_value::<PayloadSize>(json!(882)).unwrap(),
            PayloadSize::Known(882)
        );
        assert_eq!(
            serde_json::from_value::<PayloadSize>(json!(-1)).unwrap(),
            PayloadSize::Unknown
        );
        assert!(serde_json::from_value::<PayloadSize>(json!(-2)).is_err());
        assert!(serde_json::from_value::<PayloadSize>(json!(1.5)).is_err());
        assert!(serde_json::from_value::<PayloadSize>(json!("882")).is_err());
    }

    #[test]
    fn file_round_trip() {
        let packet = file_packet();
        assert_eq!(
            ShareRequest::from_packet(&packet).unwrap(),
            ShareRequest::File(file())
        );
        assert_eq!(packet.body["creationTime"], json!(1_700_000_000_000i64));
        assert_eq!(packet.body["lastModified"], json!(1_700_000_100_000i64));
        assert_eq!(packet.body["open"], json!(true));
        assert_eq!(packet.body["numberOfFiles"], json!(2));
        assert_eq!(packet.body["totalPayloadSize"], json!(2048));
        assert_eq!(
            packet.payload_offer().unwrap(),
            Some(PayloadOffer {
                size: PayloadSize::Known(882),
                port: 1739,
            })
        );
    }

    #[test]
    fn text_and_url_round_trip() {
        let text = text_request("Some text").unwrap();
        assert_eq!(
            ShareRequest::from_packet(&text).unwrap(),
            ShareRequest::Text("Some text".to_string())
        );
        assert_eq!(text.payload_offer().unwrap(), None);

        let url = url_request("https://example.com/report").unwrap();
        assert_eq!(
            ShareRequest::from_packet(&url).unwrap(),
            ShareRequest::Url("https://example.com/report".to_string())
        );
        assert_eq!(url.payload_offer().unwrap(), None);
    }

    #[test]
    fn empty_file_omits_payload_fields() {
        let packet = file_request(&empty_file(), None).unwrap();
        assert_eq!(packet.payload_size, None);
        assert_eq!(packet.payload_transfer_info, None);
        assert!(!packet.body.contains_key("open"));
        assert_eq!(
            ShareRequest::from_packet(&packet).unwrap(),
            ShareRequest::File(empty_file())
        );
        assert_eq!(packet.payload_offer().unwrap(), None);
    }

    #[test]
    fn empty_file_ignores_a_port() {
        let packet = file_request(&empty_file(), Some(1740)).unwrap();
        assert_eq!(packet.payload_size, None);
        assert_eq!(packet.payload_transfer_info, None);
        assert_eq!(packet.payload_offer().unwrap(), None);
    }

    #[test]
    fn unknown_size_uses_minus_one() {
        let file = ShareFile {
            size: PayloadSize::Unknown,
            ..file()
        };
        let packet = file_request(&file, Some(1764)).unwrap();
        assert_eq!(packet.payload_size, Some(-1));
        assert_eq!(
            serde_json::to_value(&packet).unwrap()["payloadSize"],
            json!(-1)
        );
        assert_eq!(
            ShareRequest::from_packet(&packet).unwrap(),
            ShareRequest::File(file)
        );
        assert_eq!(
            packet.payload_offer().unwrap(),
            Some(PayloadOffer {
                size: PayloadSize::Unknown,
                port: 1764,
            })
        );
        assert!(packet.payload().is_err());
    }

    #[test]
    fn non_empty_file_requires_a_port() {
        assert!(file_request(&file(), None).is_err());
    }

    #[test]
    fn builder_rejects_ports_outside_range() {
        for port in [0, 1716, 1765, u16::MAX] {
            assert!(file_request(&file(), Some(port)).is_err(), "{port}");
        }
        for port in [PAYLOAD_PORT_MIN, PAYLOAD_PORT_MAX] {
            assert!(file_request(&file(), Some(port)).is_ok(), "{port}");
        }
    }

    #[test]
    fn negative_size_is_rejected() {
        for size in [-2, -100, i64::MIN] {
            let mut packet = file_packet();
            packet.payload_size = Some(size);
            assert!(
                matches!(ShareRequest::from_packet(&packet), Err(ShareError::InvalidSize(value)) if value == size),
                "{size}"
            );
            assert!(packet.payload_offer().is_err(), "{size}");
        }
    }

    #[test]
    fn ports_outside_range_are_rejected() {
        for port in [0, 1716, 1765, u16::MAX] {
            let mut packet = file_packet();
            packet.payload_transfer_info = Some(PayloadTransferInfo::new(port));
            assert!(
                matches!(ShareRequest::from_packet(&packet), Err(ShareError::InvalidPort(value)) if value == port),
                "{port}"
            );
            assert!(packet.payload_offer().is_err(), "{port}");
        }
    }

    #[test]
    fn ports_at_the_bounds_are_accepted() {
        for port in [PAYLOAD_PORT_MIN, PAYLOAD_PORT_MAX] {
            let mut packet = file_packet();
            packet.payload_transfer_info = Some(PayloadTransferInfo::new(port));
            assert!(ShareRequest::from_packet(&packet).is_ok(), "{port}");
            assert_eq!(
                packet.payload_offer().unwrap(),
                Some(PayloadOffer {
                    size: PayloadSize::Known(882),
                    port,
                })
            );
        }
    }

    #[test]
    fn payload_fields_must_appear_together() {
        let mut packet = Packet::new(types::SHARE_REQUEST, json!({"filename": "a.png"})).unwrap();
        packet.payload_transfer_info = Some(PayloadTransferInfo::new(1739));
        assert!(ShareRequest::from_packet(&packet).is_err());
        assert!(packet.payload_offer().is_err());

        let mut packet = Packet::new(types::SHARE_REQUEST, json!({"filename": "a.png"})).unwrap();
        packet.payload_size = Some(10);
        assert!(ShareRequest::from_packet(&packet).is_err());
        assert!(packet.payload_offer().is_err());
    }

    #[test]
    fn unknown_body_fields_are_preserved() {
        let mut packet = file_packet();
        packet
            .body
            .insert("future".to_string(), json!({"nested": true}));
        assert!(matches!(
            ShareRequest::from_packet(&packet).unwrap(),
            ShareRequest::File(_)
        ));
        assert_eq!(packet.body["future"], json!({"nested": true}));
        let wire = serde_json::to_value(&packet).unwrap();
        assert_eq!(wire["body"]["future"], json!({"nested": true}));
    }

    #[test]
    fn update_round_trip() {
        for total in [0, 1_048_576] {
            let batch = ShareBatch {
                number_of_files: 3,
                total_payload_size: total,
            };
            let packet = update_request(batch.clone()).unwrap();
            assert_eq!(ShareRequest::update_from_packet(&packet).unwrap(), batch);
            assert!(matches!(
                ShareRequest::from_packet(&packet),
                Err(ShareError::NotShareRequest)
            ));
        }
    }

    #[test]
    fn update_requires_at_least_one_file() {
        assert!(update_request(ShareBatch {
            number_of_files: 0,
            total_payload_size: 10,
        })
        .is_err());

        let mut packet = update_request(ShareBatch {
            number_of_files: 1,
            total_payload_size: 10,
        })
        .unwrap();
        packet.body["numberOfFiles"] = json!(0);
        assert!(matches!(
            ShareRequest::update_from_packet(&packet),
            Err(ShareError::InvalidBatch)
        ));
    }

    #[test]
    fn batch_fields_must_appear_together() {
        for key in ["numberOfFiles", "totalPayloadSize"] {
            let mut packet = file_packet();
            packet.body.remove(key);
            assert!(
                matches!(
                    ShareRequest::from_packet(&packet),
                    Err(ShareError::InvalidBatch)
                ),
                "{key}"
            );
        }
    }

    #[test]
    fn file_without_a_batch_emits_no_batch_fields() {
        let file = ShareFile {
            batch: None,
            ..file()
        };
        let packet = file_request(&file, Some(1739)).unwrap();
        assert!(!packet.body.contains_key("numberOfFiles"));
        assert!(!packet.body.contains_key("totalPayloadSize"));
    }

    #[test]
    fn file_without_a_filename_round_trips_with_a_payload() {
        let file = ShareFile {
            filename: None,
            ..file()
        };
        let packet = file_request(&file, Some(1739)).unwrap();
        assert!(!packet.body.contains_key("filename"));
        assert_eq!(
            ShareRequest::from_packet(&packet).unwrap(),
            ShareRequest::File(file)
        );
    }

    #[test]
    fn filename_less_empty_file_has_no_wire_representation() {
        let file = ShareFile {
            filename: None,
            size: PayloadSize::Known(0),
            created_ms: None,
            modified_ms: None,
            open: false,
            batch: None,
        };
        let packet = file_request(&file, None).unwrap();
        assert!(matches!(
            ShareRequest::from_packet(&packet),
            Err(ShareError::NothingAttached)
        ));
    }

    #[test]
    fn wrong_packet_type_is_rejected() {
        let ping = Packet::empty(types::PING).unwrap();
        assert!(matches!(
            ShareRequest::from_packet(&ping),
            Err(ShareError::NotShareRequest)
        ));
        assert!(matches!(
            ShareRequest::update_from_packet(&ping),
            Err(ShareError::NotShareRequest)
        ));
        assert!(matches!(
            ShareRequest::update_from_packet(&file_packet()),
            Err(ShareError::NotShareRequest)
        ));
    }

    #[test]
    fn missing_and_multiple_bodies_are_rejected() {
        let empty = Packet::empty(types::SHARE_REQUEST).unwrap();
        assert!(matches!(
            ShareRequest::from_packet(&empty),
            Err(ShareError::NothingAttached)
        ));

        for body in [
            json!({"filename": "a.png", "text": "b"}),
            json!({"filename": "a.png", "url": "https://example.com"}),
            json!({"text": "b", "url": "https://example.com"}),
            json!({"filename": "a.png", "text": "b", "url": "https://example.com"}),
        ] {
            let packet = Packet::new(types::SHARE_REQUEST, body).unwrap();
            assert!(matches!(
                ShareRequest::from_packet(&packet),
                Err(ShareError::MultipleAttachments)
            ));
        }
    }

    #[test]
    fn malformed_bodies_do_not_panic() {
        let bodies = [
            json!({"filename": 42}),
            json!({"filename": "a", "creationTime": "yesterday"}),
            json!({"filename": "a", "lastModified": 1.5}),
            json!({"filename": "a", "open": "yes"}),
            json!({"filename": "a", "numberOfFiles": "two", "totalPayloadSize": 1}),
            json!({"filename": "a", "numberOfFiles": 1}),
            json!({"text": 7}),
            json!({"text": null}),
            json!({"url": ["https://example.com"]}),
            json!({}),
        ];
        for body in bodies {
            let packet = Packet::new(types::SHARE_REQUEST, body).unwrap();
            assert!(ShareRequest::from_packet(&packet).is_err());
        }

        let updates = [
            json!({}),
            json!({"numberOfFiles": -1, "totalPayloadSize": 0}),
            json!({"numberOfFiles": 1, "totalPayloadSize": -5}),
            json!({"numberOfFiles": "1", "totalPayloadSize": 0}),
            json!({"numberOfFiles": 1.5, "totalPayloadSize": 0}),
        ];
        for body in updates {
            let packet = Packet::new(types::SHARE_REQUEST_UPDATE, body).unwrap();
            assert!(ShareRequest::update_from_packet(&packet).is_err());
        }
    }
}
