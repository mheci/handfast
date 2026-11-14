//! Share plugin.
//!
//! Parses share requests into text, URL and file transfer intents. The plugin
//! never opens a socket or a file: the session runs every transfer and the
//! desktop adapters own the side effects.

use std::collections::{HashSet, VecDeque};

use async_trait::async_trait;
use handfast_proto::share::ShareRequest;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput, Transfer,
};

use crate::roster;

const MALFORMED_REQUEST: &str = "discarding malformed share request";
const MALFORMED_UPDATE: &str = "discarding malformed share update";
const TEXT_NOT_COPIED: &str = "share text could not be copied to the clipboard";
const URL_NOT_OPENED: &str = "share url could not be opened";
const UNSAFE_URL: &str = "discarding share url with an unsafe scheme";
const RECENT_LIMIT: usize = 64;

/// Applies inbound share requests and returns transfer intents.
#[derive(Debug, Default)]
pub struct SharePlugin {
    seen_order: VecDeque<(String, i64)>,
    seen_set: HashSet<(String, i64)>,
}

impl SharePlugin {
    /// Create a share plugin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Only non-zero ids dedupe: `id` is deprecated and senders may leave it at 0.
    fn is_duplicate(&mut self, packet: &Packet) -> bool {
        if packet.id == 0
            || !matches!(
                packet.ptype.as_str(),
                types::SHARE_REQUEST | types::SHARE_REQUEST_UPDATE
            )
        {
            return false;
        }
        let key = (packet.ptype.clone(), packet.id);
        if !self.seen_set.insert(key.clone()) {
            return true;
        }
        self.seen_order.push_back(key);
        if self.seen_order.len() > RECENT_LIMIT {
            if let Some(expired) = self.seen_order.pop_front() {
                self.seen_set.remove(&expired);
            }
        }
        false
    }
}

fn warn(message: &str) -> PluginEvent {
    PluginEvent::Log {
        level: LogLevel::Warn,
        message: message.to_string(),
    }
}

fn malformed(message: &str) -> PluginOutput {
    PluginOutput::new().with_event(warn(message))
}

/// Only `http`, `https` and `mailto` are opened, with a scheme made of ASCII
/// scheme bytes. The original string is passed through unchanged.
fn is_safe_url(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return false;
    }
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "mailto"
    )
}

#[async_trait]
impl Plugin for SharePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SHARE
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if self.is_duplicate(packet) {
            return Ok(PluginOutput::new());
        }
        let output = match packet.ptype.as_str() {
            types::SHARE_REQUEST => match ShareRequest::from_packet(packet) {
                Ok(ShareRequest::File(file)) => match packet.payload_offer() {
                    Ok(offer) => PluginOutput::new().with_transfer(Transfer::ReceiveFile {
                        filename: file.filename,
                        size: file.size,
                        offer,
                    }),
                    Err(_) => malformed(MALFORMED_REQUEST),
                },
                Ok(ShareRequest::Text(text)) => {
                    let copied = match ctx.desktop.clipboard.as_ref() {
                        Some(clipboard) => clipboard.write_text(&text).await.is_ok(),
                        None => false,
                    };
                    let output = PluginOutput::new();
                    let output = if copied {
                        output
                    } else {
                        output.with_event(warn(TEXT_NOT_COPIED))
                    };
                    output.with_event(PluginEvent::ShareText { text })
                }
                Ok(ShareRequest::Url(url)) => {
                    if !is_safe_url(&url) {
                        return Ok(malformed(UNSAFE_URL));
                    }
                    let opened = match ctx.desktop.shell.as_ref() {
                        Some(shell) => shell.open_url(&url).await.is_ok(),
                        None => false,
                    };
                    let output = PluginOutput::new();
                    let output = if opened {
                        output
                    } else {
                        output.with_event(warn(URL_NOT_OPENED))
                    };
                    output.with_event(PluginEvent::ShareUrl { url })
                }
                Err(_) => malformed(MALFORMED_REQUEST),
            },
            types::SHARE_REQUEST_UPDATE => match ShareRequest::update_from_packet(packet) {
                Ok(batch) => PluginOutput::new().with_event(PluginEvent::ShareBatch {
                    number_of_files: batch.number_of_files,
                    total_payload_size: batch.total_payload_size,
                }),
                Err(_) => malformed(MALFORMED_UPDATE),
            },
            _ => PluginOutput::new(),
        };
        Ok(output)
    }
}

/// Builds share plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShareFactory;

impl PluginFactory for ShareFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SHARE
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(SharePlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::{Desktop, MockDesktop};
    use handfast_proto::share::{
        file_request, text_request, update_request, url_request, PayloadOffer, PayloadSize,
        ShareBatch, ShareFile,
    };
    use handfast_proto::{DeviceId, DeviceName, DeviceType, PayloadTransferInfo};
    use handfast_runtime::Device;
    use serde_json::json;

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Desktop,
        )
    }

    async fn run(desktop: &Desktop, packet: &Packet) -> PluginOutput {
        run_plugin(&mut SharePlugin::new(), desktop, packet).await
    }

    async fn run_plugin(
        plugin: &mut SharePlugin,
        desktop: &Desktop,
        packet: &Packet,
    ) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        plugin.on_packet(&mut ctx, packet).await.unwrap()
    }

    fn file() -> ShareFile {
        ShareFile {
            filename: Some("report.pdf".to_string()),
            size: PayloadSize::Known(882),
            created_ms: None,
            modified_ms: None,
            open: false,
            batch: None,
        }
    }

    #[tokio::test]
    async fn text_is_copied_and_emitted() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let output = run(&desktop, &text_request("Some text").unwrap()).await;
        assert_eq!(mock.clipboard().as_deref(), Some("Some text"));
        assert_eq!(
            output.events,
            vec![PluginEvent::ShareText {
                text: "Some text".to_string(),
            }]
        );
        assert!(output.transfers.is_empty());
    }

    #[tokio::test]
    async fn text_without_clipboard_logs_and_still_emits() {
        let output = run(&Desktop::default(), &text_request("Some text").unwrap()).await;
        assert!(matches!(
            output.events.as_slice(),
            [
                PluginEvent::Log {
                    level: LogLevel::Warn,
                    message,
                },
                PluginEvent::ShareText { text },
            ] if message == TEXT_NOT_COPIED && text == "Some text"
        ));
    }

    #[tokio::test]
    async fn url_opens_http_https_and_mailto() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let urls = [
            "http://example.com/a",
            "https://example.com/b",
            "mailto:user@example.com",
        ];
        let mut events = Vec::new();
        for url in urls {
            let output = run(&desktop, &url_request(url).unwrap()).await;
            events.extend(output.events);
        }
        assert_eq!(mock.shell_urls(), urls);
        assert_eq!(
            events,
            urls.map(|url| PluginEvent::ShareUrl {
                url: url.to_string(),
            })
            .to_vec()
        );
    }

    #[tokio::test]
    async fn unsafe_url_schemes_are_dropped() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let urls = [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/plain,hi",
            "ftp://example.com",
            " https://example.com",
            "example.com",
        ];
        for url in urls {
            let output = run(&desktop, &url_request(url).unwrap()).await;
            assert!(
                matches!(
                    output.events.as_slice(),
                    [PluginEvent::Log {
                        level: LogLevel::Warn,
                        message,
                    }] if message == UNSAFE_URL
                ),
                "{url}"
            );
            assert!(output.transfers.is_empty(), "{url}");
        }
        assert!(mock.shell_urls().is_empty());
    }

    #[tokio::test]
    async fn url_without_shell_logs_and_still_emits() {
        let output = run(
            &Desktop::default(),
            &url_request("https://example.com").unwrap(),
        )
        .await;
        assert!(matches!(
            output.events.as_slice(),
            [
                PluginEvent::Log {
                    level: LogLevel::Warn,
                    message,
                },
                PluginEvent::ShareUrl { url },
            ] if message == URL_NOT_OPENED && url == "https://example.com"
        ));
    }

    #[tokio::test]
    async fn file_returns_receive_intent() {
        let output = run(
            &Desktop::default(),
            &file_request(&file(), Some(1739)).unwrap(),
        )
        .await;
        assert_eq!(
            output.transfers,
            vec![Transfer::ReceiveFile {
                filename: Some("report.pdf".to_string()),
                size: PayloadSize::Known(882),
                offer: Some(PayloadOffer {
                    size: PayloadSize::Known(882),
                    port: 1739,
                }),
            }]
        );
        assert!(output.events.is_empty());
        assert!(output.packets.is_empty());
    }

    #[tokio::test]
    async fn empty_file_is_known_zero_without_offer() {
        let empty = ShareFile {
            size: PayloadSize::Known(0),
            ..file()
        };
        let output = run(&Desktop::default(), &file_request(&empty, None).unwrap()).await;
        assert_eq!(
            output.transfers,
            vec![Transfer::ReceiveFile {
                filename: Some("report.pdf".to_string()),
                size: PayloadSize::Known(0),
                offer: None,
            }]
        );
    }

    #[tokio::test]
    async fn unknown_size_keeps_unknown_offer() {
        let unknown = ShareFile {
            size: PayloadSize::Unknown,
            ..file()
        };
        let output = run(
            &Desktop::default(),
            &file_request(&unknown, Some(1764)).unwrap(),
        )
        .await;
        assert_eq!(
            output.transfers,
            vec![Transfer::ReceiveFile {
                filename: Some("report.pdf".to_string()),
                size: PayloadSize::Unknown,
                offer: Some(PayloadOffer {
                    size: PayloadSize::Unknown,
                    port: 1764,
                }),
            }]
        );
    }

    #[tokio::test]
    async fn update_emits_share_batch() {
        let output = run(
            &Desktop::default(),
            &update_request(ShareBatch {
                number_of_files: 3,
                total_payload_size: 1_048_576,
            })
            .unwrap(),
        )
        .await;
        assert_eq!(
            output.events,
            vec![PluginEvent::ShareBatch {
                number_of_files: 3,
                total_payload_size: 1_048_576,
            }]
        );
        assert!(output.transfers.is_empty());
    }

    #[tokio::test]
    async fn malformed_packets_log_and_do_not_fail() {
        let mixed = Packet::new(
            types::SHARE_REQUEST,
            json!({"filename": "a.png", "text": "b"}),
        )
        .unwrap();
        let mut bad_port = file_request(&file(), Some(1739)).unwrap();
        bad_port.payload_transfer_info = Some(PayloadTransferInfo::new(1765));
        let bad_update = Packet::new(
            types::SHARE_REQUEST_UPDATE,
            json!({"numberOfFiles": 0, "totalPayloadSize": 1}),
        )
        .unwrap();
        let cases = [
            (
                Packet::empty(types::SHARE_REQUEST).unwrap(),
                MALFORMED_REQUEST,
            ),
            (mixed, MALFORMED_REQUEST),
            (bad_port, MALFORMED_REQUEST),
            (bad_update, MALFORMED_UPDATE),
        ];
        for (packet, message) in cases {
            let output = run(&Desktop::default(), &packet).await;
            assert_eq!(
                output.events,
                vec![PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: message.to_string(),
                }],
                "{}",
                packet.ptype
            );
            assert!(output.transfers.is_empty(), "{}", packet.ptype);
            assert!(output.packets.is_empty(), "{}", packet.ptype);
        }
    }

    #[tokio::test]
    async fn non_share_packet_is_ignored() {
        let output = run(&Desktop::default(), &Packet::empty(types::PING).unwrap()).await;
        assert_eq!(output, PluginOutput::new());
    }

    #[tokio::test]
    async fn duplicate_packets_are_dropped() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = SharePlugin::new();

        let mut url = url_request("https://example.com").unwrap();
        url.id = 1;
        assert_eq!(
            run_plugin(&mut plugin, &desktop, &url).await.events.len(),
            1
        );
        assert_eq!(
            run_plugin(&mut plugin, &desktop, &url).await,
            PluginOutput::new()
        );
        assert_eq!(mock.shell_urls(), ["https://example.com"]);

        let mut file_packet = file_request(&file(), Some(1739)).unwrap();
        file_packet.id = 2;
        assert_eq!(
            run_plugin(&mut plugin, &desktop, &file_packet)
                .await
                .transfers
                .len(),
            1
        );
        assert!(run_plugin(&mut plugin, &desktop, &file_packet)
            .await
            .transfers
            .is_empty());

        let mut text = text_request("again").unwrap();
        text.id = 3;
        assert_eq!(
            run_plugin(&mut plugin, &desktop, &text).await.events.len(),
            1
        );
        assert!(run_plugin(&mut plugin, &desktop, &text)
            .await
            .events
            .is_empty());

        let mut update = update_request(ShareBatch {
            number_of_files: 2,
            total_payload_size: 4096,
        })
        .unwrap();
        update.id = 4;
        assert_eq!(
            run_plugin(&mut plugin, &desktop, &update)
                .await
                .events
                .len(),
            1
        );
        assert!(run_plugin(&mut plugin, &desktop, &update)
            .await
            .events
            .is_empty());
    }

    #[tokio::test]
    async fn different_ids_and_types_are_applied() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = SharePlugin::new();

        let mut first = url_request("https://example.com/a").unwrap();
        first.id = 1;
        let mut second = url_request("https://example.com/b").unwrap();
        second.id = 2;
        run_plugin(&mut plugin, &desktop, &first).await;
        run_plugin(&mut plugin, &desktop, &second).await;
        assert_eq!(
            mock.shell_urls(),
            ["https://example.com/a", "https://example.com/b"]
        );

        let mut update = update_request(ShareBatch {
            number_of_files: 1,
            total_payload_size: 10,
        })
        .unwrap();
        update.id = 1;
        assert_eq!(
            run_plugin(&mut plugin, &desktop, &update)
                .await
                .events
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn zero_id_packets_are_applied_again() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = SharePlugin::new();
        let url = url_request("https://example.com").unwrap();
        assert_eq!(url.id, 0);
        assert_eq!(
            run_plugin(&mut plugin, &desktop, &url).await.events.len(),
            1
        );
        assert_eq!(
            run_plugin(&mut plugin, &desktop, &url).await.events.len(),
            1
        );
        assert_eq!(mock.shell_urls().len(), 2);
    }

    #[tokio::test]
    async fn duplicate_ids_are_forgotten_after_the_bound() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = SharePlugin::new();
        let mut url = url_request("https://example.com").unwrap();

        url.id = 1;
        run_plugin(&mut plugin, &desktop, &url).await;
        assert!(run_plugin(&mut plugin, &desktop, &url)
            .await
            .events
            .is_empty());

        for id in 2..=RECENT_LIMIT as i64 {
            url.id = id;
            run_plugin(&mut plugin, &desktop, &url).await;
        }
        assert!(run_plugin(&mut plugin, &desktop, &url)
            .await
            .events
            .is_empty());

        url.id = RECENT_LIMIT as i64 + 1;
        run_plugin(&mut plugin, &desktop, &url).await;
        url.id = 1;
        assert_eq!(
            run_plugin(&mut plugin, &desktop, &url).await.events.len(),
            1
        );
        assert_eq!(mock.shell_urls().len(), RECENT_LIMIT + 2);
    }
}
