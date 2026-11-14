//! Redacted daemon log lines for plugin events.
//!
//! Clipboard text, share text and URLs are peer-supplied bodies. They are
//! logged by type and byte length only. Transfer metadata (ids, byte counts,
//! paths, failure reasons and batch counts) is safe to log. No event is ever
//! formatted with `Debug`.

use handfast_proto::DeviceId;
use handfast_runtime::PluginEvent;

/// Log one batch of plugin events without their text bodies.
pub fn log_plugin_events(device_id: &DeviceId, events: &[PluginEvent]) {
    tracing::info!(%device_id, count = events.len(), "plugin events");
    for event in events {
        log_plugin_event(device_id, event);
    }
}

fn log_plugin_event(device_id: &DeviceId, event: &PluginEvent) {
    match event {
        PluginEvent::Ping { message } => {
            tracing::info!(
                %device_id,
                event = "Ping",
                bytes = message.as_ref().map_or(0, String::len),
                "plugin event"
            );
        }
        PluginEvent::Battery(battery) => {
            tracing::info!(
                %device_id,
                event = "Battery",
                charge = ?battery.charge,
                charging = battery.charging,
                threshold = ?battery.threshold,
                "plugin event"
            );
        }
        PluginEvent::Clipboard { text, .. } => {
            tracing::info!(
                %device_id,
                event = "Clipboard",
                bytes = text.len(),
                "plugin event"
            );
        }
        PluginEvent::ShareText { text } => {
            tracing::info!(
                %device_id,
                event = "ShareText",
                bytes = text.len(),
                "plugin event"
            );
        }
        PluginEvent::ShareUrl { url } => {
            tracing::info!(
                %device_id,
                event = "ShareUrl",
                bytes = url.len(),
                "plugin event"
            );
        }
        PluginEvent::ShareBatch {
            number_of_files,
            total_payload_size,
        } => {
            tracing::info!(
                %device_id,
                event = "ShareBatch",
                number_of_files = *number_of_files,
                total_payload_size = *total_payload_size,
                "plugin event"
            );
        }
        PluginEvent::ShareProgress {
            transfer_id,
            bytes,
            total,
        } => {
            tracing::info!(
                %device_id,
                event = "ShareProgress",
                transfer_id = *transfer_id,
                bytes = *bytes,
                total = ?total,
                "plugin event"
            );
        }
        PluginEvent::ShareCompleted {
            transfer_id,
            path,
            bytes,
        } => {
            tracing::info!(
                %device_id,
                event = "ShareCompleted",
                transfer_id = *transfer_id,
                bytes = *bytes,
                path = %path.display(),
                "plugin event"
            );
        }
        PluginEvent::ShareFailed {
            transfer_id,
            reason,
        } => {
            tracing::info!(
                %device_id,
                event = "ShareFailed",
                transfer_id = *transfer_id,
                reason = %reason,
                "plugin event"
            );
        }
        PluginEvent::Log { level, message } => {
            // Plugin log messages are local diagnostics, not peer bodies, but
            // some plugins build them from peer fields. Escape control bytes so
            // a peer cannot forge or split a log line.
            let message = sanitize_message(message);
            tracing::info!(
                %device_id,
                event = "Log",
                level = ?level,
                message = %message,
                "plugin event"
            );
        }
    }
}

/// Escape control and non-printable characters in a plugin log message.
///
/// The escape set matches `char::escape_default`: tab, carriage return and line
/// feed get their short escapes, `\\`, `'` and `"` get a leading backslash,
/// printable ASCII passes through, and every other character becomes
/// `\u{...}`. A peer-supplied field therefore cannot inject a newline or a
/// terminal sequence into the daemon log.
fn sanitize_message(message: &str) -> String {
    use std::fmt::Write as _;

    let bytes = message.as_bytes();
    let mut escaped = String::with_capacity(bytes.len());
    let mut run_start = 0;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if matches!(byte, b' '..=b'~') && !matches!(byte, b'\\' | b'\'' | b'"') {
            index += 1;
            continue;
        }
        escaped.push_str(&message[run_start..index]);
        let c = message[index..]
            .chars()
            .next()
            .expect("index is always a char boundary");
        match c {
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            '\n' => escaped.push_str("\\n"),
            '\\' | '\'' | '"' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => {
                let code = c as u32;
                let _ = write!(escaped, "\\u{{{code:x}}}");
            }
        }
        index += c.len_utf8();
        run_start = index;
    }
    escaped.push_str(&message[run_start..]);
    escaped
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use handfast_proto::DeviceId;
    use handfast_runtime::{LogLevel, PluginEvent};

    use super::{log_plugin_events, sanitize_message};

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("log buffer poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn clipboard_share_text_and_share_url_logs_omit_the_body() {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();

        let clipboard = "SECRET-CLIPBOARD";
        let text = "SECRET-SHARE-TEXT";
        let url = "https://example.com/SECRET-URL";
        let events = [
            PluginEvent::Clipboard {
                text: clipboard.to_string(),
                timestamp: Some(1),
            },
            PluginEvent::ShareText {
                text: text.to_string(),
            },
            PluginEvent::ShareUrl {
                url: url.to_string(),
            },
        ];
        let device_id = DeviceId::new("00112233445566778899aabbccddeeff").expect("a valid ID");

        tracing::subscriber::with_default(subscriber, || {
            log_plugin_events(&device_id, &events);
        });

        let log = String::from_utf8(buffer.0.lock().expect("the log buffer is readable").clone())
            .expect("the log is UTF-8");
        assert!(!log.contains(clipboard), "{log}");
        assert!(!log.contains(text), "{log}");
        assert!(!log.contains("SECRET-URL"), "{log}");
        assert!(log.contains("event=\"Clipboard\""), "{log}");
        assert!(log.contains(&format!("bytes={}", clipboard.len())), "{log}");
        assert!(log.contains("event=\"ShareText\""), "{log}");
        assert!(log.contains(&format!("bytes={}", text.len())), "{log}");
        assert!(log.contains("event=\"ShareUrl\""), "{log}");
        assert!(log.contains(&format!("bytes={}", url.len())), "{log}");
    }

    #[test]
    fn log_message_escapes_control_characters() {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();

        let message = "line\ncarriage\rnull\0escape\u{1b}[31m bidi\u{202e}";
        let events = [PluginEvent::Log {
            level: LogLevel::Info,
            message: message.to_string(),
        }];
        let device_id = DeviceId::new("00112233445566778899aabbccddeeff").expect("a valid ID");

        tracing::subscriber::with_default(subscriber, || {
            log_plugin_events(&device_id, &events);
        });

        let log = String::from_utf8(buffer.0.lock().expect("the log buffer is readable").clone())
            .expect("the log is UTF-8");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "the message must not add a log line: {log}");
        for line in &lines {
            assert!(
                !line.chars().any(char::is_control),
                "raw control character in the log: {log}"
            );
            assert!(!line.contains('\u{202e}'), "raw bidi control: {log}");
        }
        assert!(log.contains("\\n"), "{log}");
        assert!(log.contains("\\r"), "{log}");
        assert!(log.contains("\\u{0}") || log.contains("\\0"), "{log}");
        assert!(log.contains("\\u{1b}"), "{log}");
        assert!(log.contains("\\u{202e}"), "{log}");
    }

    #[test]
    fn sanitize_message_matches_escape_default() {
        let cases = [
            ("", ""),
            ("plain ascii", "plain ascii"),
            ("spaces and ~", "spaces and ~"),
            ("tab\there", "tab\\there"),
            ("line\nfeed", "line\\nfeed"),
            ("carriage\rreturn", "carriage\\rreturn"),
            ("null\0byte", "null\\u{0}byte"),
            ("bell\u{7}", "bell\\u{7}"),
            ("esc\u{1b}[31mred", "esc\\u{1b}[31mred"),
            ("delete\u{7f}", "delete\\u{7f}"),
            ("c1\u{80}\u{9f}", "c1\\u{80}\\u{9f}"),
            ("bidi\u{202e}override", "bidi\\u{202e}override"),
            ("iso\u{2066}late\u{2069}", "iso\\u{2066}late\\u{2069}"),
            ("quote'and\"double", "quote\\'and\\\"double"),
            ("back\\slash", "back\\\\slash"),
            ("caf\u{e9}", "caf\\u{e9}"),
            ("latin\u{100}", "latin\\u{100}"),
            ("emoji \u{1f600}", "emoji \\u{1f600}"),
            ("max \u{10ffff}", "max \\u{10ffff}"),
            ("\u{1b}[0m\u{202e}\u{9f}", "\\u{1b}[0m\\u{202e}\\u{9f}"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_message(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn sanitize_message_keeps_long_safe_runs_in_bulk() {
        let safe_run = "the quick brown fox jumps over the lazy dog 0123456789 ".repeat(2000);
        assert_eq!(sanitize_message(&safe_run), safe_run);

        let mut large = String::with_capacity(safe_run.len() + 4096);
        for round in 0..1000 {
            large.push_str(&safe_run[..512]);
            large.push(['\n', '\u{1b}', '\u{202e}', '\u{7f}'][round % 4]);
        }
        let expected: String = large.chars().flat_map(char::escape_default).collect();
        assert_eq!(sanitize_message(&large), expected);
    }

    #[test]
    fn sanitize_message_matches_escape_default_for_every_char() {
        let all: String = (0..=0x10ffffu32).filter_map(char::from_u32).collect();
        let expected: String = all.chars().flat_map(char::escape_default).collect();
        assert_eq!(sanitize_message(&all), expected);
    }
}
