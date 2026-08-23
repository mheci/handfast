//! Pure rendering for the interactive interface.
//!
//! Every function here takes an immutable [`State`] snapshot plus a ratatui
//! [`Frame`] and draws — no I/O, no mutation — so visuals can be exercised
//! alongside the reducers in `state.rs` through plain unit tests.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, Wrap},
    Frame,
};

use crate::model::{ellipsize, human_bytes, short_hash};
use crate::state::{State, Tab, LOG_CAP, NOTIFICATION_CAP};

/// Width of the fixed file-name column on the Transfers tab.
const TRANSFER_NAME_WIDTH: usize = 22;

/// Render the whole interface for one frame.
pub(crate) fn draw(f: &mut Frame<'_>, state: &State) {
    let full = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(full);

    draw_header(f, chunks[0], state);
    draw_tabs(f, chunks[1], state);
    match state.tab {
        Tab::Devices => draw_devices(f, chunks[2], state),
        Tab::Transfers => draw_transfers(f, chunks[2], state),
        Tab::Notifications => draw_notifications(f, chunks[2], state),
        Tab::Logs => draw_logs(f, chunks[2], state),
        Tab::Help => draw_help_tab(f, chunks[2]),
    }
    draw_footer(f, chunks[3], state);

    if state.help_overlay {
        draw_help_overlay(f, full);
    }
    // The reply modal sits on top of everything (help cannot be open while
    // composing: `?` types into the draft).
    if state.reply_open {
        draw_reply_modal(f, full, state);
    }
}

/// One-line header: binary name plus daemon identity/shutdown status.
fn draw_header(f: &mut Frame<'_>, area: Rect, state: &State) {
    let mut spans = vec![Span::styled(
        " hfctl",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(daemon) = &state.daemon {
        spans.push(Span::raw(format!(
            " · {} · pid {} · protocol v{}",
            daemon.app, daemon.pid, daemon.version
        )));
    }
    if state.shutdown {
        spans.push(Span::styled(
            " · DAEMON SHUTTING DOWN",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// One-line tab bar with the active tab inverted.
fn draw_tabs(f: &mut Frame<'_>, area: Rect, state: &State) {
    let mut spans = Vec::with_capacity(Tab::ALL.len() * 2);
    for tab in Tab::ALL {
        let label = format!(" {} ", tab.label());
        let style = if tab == state.tab {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Devices tab: device table, optionally split with the plugin detail panel.
fn draw_devices(f: &mut Frame<'_>, area: Rect, state: &State) {
    if state.devices.is_empty() {
        render_empty(
            f,
            area,
            "no devices discovered yet — waiting for discovery events",
        );
        return;
    }

    let (table_area, detail_area) = if state.detail_open {
        let panel_rows = u16::try_from(state.plugins.len().min(10))
            .unwrap_or(10)
            .saturating_add(2);
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(panel_rows)])
            .split(area);
        (split[0], Some(split[1]))
    } else {
        (area, None)
    };

    let rows = state.devices.iter().enumerate().map(|(index, device)| {
        let style = if index == state.device_cursor {
            row_selected_style()
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(device.name.clone()),
            Cell::from(device.kind.clone()),
            badge_cell(device.paired),
            Cell::from(short_hash(&device.id)),
        ])
        .style(style)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(35),
            Constraint::Percentage(15),
            Constraint::Percentage(15),
            Constraint::Percentage(35),
        ],
    )
    .header(Row::new(["NAME", "TYPE", "STATE", "ID"]).style(header_style()))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Devices ({}) ", state.devices.len())),
    );
    f.render_widget(table, table_area);

    if let Some(panel) = detail_area {
        draw_plugins(f, panel, state);
    }
}

/// Pairing badge cell mandated for device rows.
fn badge_cell(paired: bool) -> Cell<'static> {
    if paired {
        Cell::from("[paired]").style(Style::default().fg(Color::Green))
    } else {
        Cell::from("[found]").style(Style::default().fg(Color::Yellow))
    }
}

/// Plugin detail panel below the device table.
fn draw_plugins(f: &mut Frame<'_>, area: Rect, state: &State) {
    let title = match &state.detail_device {
        Some(id) => format!(" Plugins of {} — Space toggles ", short_hash(id)),
        None => " Plugins ".to_owned(),
    };
    if state.plugins.is_empty() {
        render_empty(f, area, "loading plugins… (or none reported)");
        return;
    }

    let items = state.plugins.iter().enumerate().map(|(index, plugin)| {
        let checkbox = if plugin.enabled { "[x]" } else { "[ ]" };
        let line = Line::from(vec![
            Span::raw(checkbox),
            Span::raw(" "),
            Span::raw(plugin.title.clone()),
            Span::styled(
                format!(" — {}", plugin.name),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        let style = if Some(index) == state.selected_plugin_index() {
            row_selected_style()
        } else {
            Style::default()
        };
        ListItem::new(line).style(style)
    });

    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(list, area);
}

/// Transfers tab: hand-drawn block-character progress bars.
fn draw_transfers(f: &mut Frame<'_>, area: Rect, state: &State) {
    if state.transfers.is_empty() {
        render_empty(f, area, "no transfers yet");
        return;
    }

    // Fixed overhead: id column (short hash) + gaps + name column +
    // percent/byte stats.
    const ID_WIDTH: usize = 8;
    let overhead = ID_WIDTH + 2 + TRANSFER_NAME_WIDTH + 2 + " 100%".len() + "  0 B / 0 B".len();
    let bar_width = usize::from(area.width)
        .saturating_sub(overhead)
        .clamp(4, 40);

    let items = state
        .transfer_rows()
        .into_iter()
        .enumerate()
        .map(|(index, (id, entry))| {
            let bar_style = Style::default().fg(if entry.is_finished() {
                Color::Green
            } else {
                Color::Cyan
            });
            let line = Line::from(vec![
                Span::styled(short_hash(id), Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::raw(format!(
                    "{name:<width$}",
                    name = ellipsize(entry.display_name(), TRANSFER_NAME_WIDTH),
                    width = TRANSFER_NAME_WIDTH,
                )),
                Span::styled(progress_bar(entry.done, entry.total, bar_width), bar_style),
                Span::raw(format!(
                    " {:>3}%  {} / {}",
                    percent_of(entry.done, entry.total),
                    human_bytes(entry.done),
                    human_bytes(entry.total),
                )),
            ]);
            let style = if index == state.transfer_cursor {
                row_selected_style()
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        });

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Transfers ({}) ", state.transfers.len())),
    );
    f.render_widget(list, area);
}

/// Notifications tab: ring-buffer contents, oldest first.
fn draw_notifications(f: &mut Frame<'_>, area: Rect, state: &State) {
    if state.notifications.is_empty() {
        render_empty(f, area, "no notifications mirrored yet");
        return;
    }

    let items = state.notifications.iter().enumerate().map(|(index, row)| {
        let line = Line::from(vec![
            Span::styled(short_hash(&row.id), Style::default().fg(Color::DarkGray)),
            Span::raw(" "),
            Span::styled(row.app.clone(), Style::default().fg(Color::Magenta)),
            Span::raw(": "),
            Span::styled(
                row.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" — "),
            Span::raw(row.body.clone()),
        ]);
        let style = if index == state.notification_cursor {
            row_selected_style()
        } else {
            Style::default()
        };
        ListItem::new(line).style(style)
    });

    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(format!(
        " Notifications ({}/{NOTIFICATION_CAP}) ",
        state.notifications.len()
    )));
    f.render_widget(list, area);
}

/// Logs tab: auto-following tail of the rolling buffer.
fn draw_logs(f: &mut Frame<'_>, area: Rect, state: &State) {
    if state.logs.is_empty() {
        render_empty(f, area, "no log records received yet");
        return;
    }

    // Follow the tail: skip everything above the visible window.
    let inner = usize::from(area.height.saturating_sub(2)).max(1);
    let skip = state.logs.len().saturating_sub(inner);
    let lines: Vec<Line<'_>> = state
        .logs
        .iter()
        .skip(skip)
        .map(|entry| Line::from(entry.clone()))
        .collect();

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Logs ({}/{LOG_CAP} buffered) ", state.logs.len())),
    );
    f.render_widget(paragraph, area);
}

/// Full-page inline keybinding reference.
fn draw_help_tab(f: &mut Frame<'_>, area: Rect) {
    let page = Paragraph::new(help_lines())
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" Help "));
    f.render_widget(page, area);
}

/// Floating help overlay drawn over whatever tab is active.
fn draw_help_overlay(f: &mut Frame<'_>, full: Rect) {
    let popup = centered_rect(72, 85, full);
    f.render_widget(Clear, popup);
    let page = Paragraph::new(help_lines())
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Keybindings (? closes) ")
                .border_style(Style::default().fg(Color::Cyan)),
        );
    f.render_widget(page, popup);
}

/// Reply modal: one-line editor for the notification reply draft.
///
/// The underscore is a stand-in caret; Enter submits via the
/// `ReplyToNotification` action and Esc cancels (see `state.rs`).
fn draw_reply_modal(f: &mut Frame<'_>, full: Rect, state: &State) {
    let popup = centered_rect(60, 25, full);
    f.render_widget(Clear, popup);

    let subject = state.reply_subject();
    let title = match subject {
        Some(row) if row.title.is_empty() => format!(" Reply — {} ", short_hash(&row.id)),
        Some(row) => format!(" Reply — {}: {} ", row.app, ellipsize(&row.title, 32)),
        None => " Reply ".to_owned(),
    };

    let page = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Magenta)),
            Span::raw(state.reply_draft.clone()),
            Span::styled("_", Style::default().fg(Color::Magenta)),
        ]),
        Line::default(),
        Line::from(Span::styled(
            "Enter send · Esc cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .wrap(Wrap { trim: false })
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Magenta)),
    );
    f.render_widget(page, popup);
}

/// Footer: transient feedback chip plus the static key hints.
fn draw_footer(f: &mut Frame<'_>, area: Rect, state: &State) {
    let mut spans = Vec::with_capacity(4);
    if let Some(message) = &state.flash {
        spans.push(Span::styled(
            format!(" {message} "),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ));
    }
    spans.push(Span::styled(
        " Tab switch · j/k move · p pair · u unpair · r reply · Enter detail · Space toggle · ? help · q quit",
        Style::default().fg(Color::DarkGray),
    ));
    if let Some(clip) = &state.clipboard {
        spans.push(Span::styled(
            format!(" · remote clipboard: {} chars", clip.chars().count()),
            Style::default().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Dim placeholder message centered nowhere in particular (top-left of area).
fn render_empty(f: &mut Frame<'_>, area: Rect, text: &str) {
    let page = Paragraph::new(text.to_owned()).style(Style::default().fg(Color::DarkGray));
    f.render_widget(page, area);
}

/// Shared style for table/list headers.
fn header_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Shared style highlighting the cursor row/item.
fn row_selected_style() -> Style {
    Style::default().bg(Color::DarkGray)
}

/// Keybinding reference shared by the Help tab and the `?` overlay.
#[must_use]
pub(crate) fn help_lines() -> Vec<Line<'static>> {
    const ROWS: [(&str, &str); 12] = [
        (
            "Tab / Shift+Tab",
            "switch tabs (Devices/Transfers/Notifications/Logs/Help)",
        ),
        ("j / Down, k / Up", "move the selection down/up"),
        ("g / Home, G / End", "jump to the first / last row"),
        ("p", "pair the selected device"),
        ("u", "unpair the selected device"),
        ("r", "reply to the selected notification (opens a modal)"),
        (
            "Enter",
            "open/close device detail · send the reply draft in the modal",
        ),
        ("Space", "toggle the focused plugin in the detail panel"),
        (
            "printable keys",
            "type into the reply modal; Backspace deletes",
        ),
        ("?", "toggle this help overlay"),
        (
            "Esc",
            "cancel/close: reply modal, help overlay, then detail panel",
        ),
        ("q / Ctrl+C", "quit hfctl"),
    ];

    let mut lines = Vec::with_capacity(ROWS.len() + 4);
    for (keys, description) in ROWS {
        lines.push(Line::from(vec![
            Span::styled(format!("{keys:<22}"), Style::default().fg(Color::Cyan)),
            Span::raw(description.to_owned()),
        ]));
    }
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        format!(
            "Buffers: notifications keep {NOTIFICATION_CAP} entries, logs keep \
             {LOG_CAP} lines (oldest dropped first)."
        ),
        Style::default().fg(Color::DarkGray),
    )));
    lines
}

/// Hand-drawn progress bar: `█` for completed fraction, `░` for the rest.
///
/// Uses integer math so results are deterministic; `total == 0` renders an
/// entirely empty bar rather than dividing by zero.
#[must_use]
pub(crate) fn progress_bar(done: u64, total: u64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let done = done.min(total);
    let filled = if total == 0 {
        0usize
    } else {
        let scaled = u128::from(done) * u128::from(u64::try_from(width).unwrap_or(u64::MAX));
        let ratio = scaled / u128::from(total);
        usize::try_from(ratio).unwrap_or(width)
    };
    let filled = filled.min(width);

    let mut bar = String::with_capacity(width * char_len_of_block());
    for _ in 0..filled {
        bar.push('█');
    }
    for _ in filled..width {
        bar.push('░');
    }
    bar
}

/// UTF-8 length of the block characters pushed by [`progress_bar`].
fn char_len_of_block() -> usize {
    '█'.len_utf8()
}

/// Integer percentage of `done/total`, clamped to `0..=100`.
#[must_use]
pub(crate) fn percent_of(done: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    done.min(total).saturating_mul(100) / total
}

/// Classic three-way percentage split producing a centered sub-rectangle.
fn centered_rect(percent_x: u16, percent_y: u16, outer: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(outer);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_bar_fills_exactly() {
        assert_eq!(progress_bar(0, 100, 10), "░░░░░░░░░░");
        assert_eq!(progress_bar(50, 100, 10), "█████░░░░░");
        assert_eq!(progress_bar(100, 100, 10), "██████████");
        assert_eq!(progress_bar(1, 3, 10), "███░░░░░░░");
        // Over-progress clamps to a full bar.
        assert_eq!(progress_bar(150, 100, 10), "██████████");
        // Zero-total renders an empty bar instead of panicking.
        assert_eq!(progress_bar(5, 0, 10), "░░░░░░░░░░");
        // Degenerate width.
        assert_eq!(progress_bar(50, 100, 0), "");
    }

    #[test]
    fn percent_of_clamps() {
        assert_eq!(percent_of(0, 0), 0);
        assert_eq!(percent_of(50, 100), 50);
        assert_eq!(percent_of(99, 100), 99);
        assert_eq!(percent_of(150, 100), 100);
        assert_eq!(percent_of(7, 0), 0);
    }

    #[test]
    fn centered_rect_stays_inside_outer() {
        for width in [0u16, 1, 13, 80, 200] {
            for height in [0u16, 1, 7, 24, 60] {
                let outer = Rect::new(0, 0, width, height);
                let inner = centered_rect(70, 80, outer);
                assert!(inner.x >= outer.x);
                assert!(inner.y >= outer.y);
                assert!(inner.right() <= outer.right());
                assert!(inner.bottom() <= outer.bottom());
            }
        }
    }

    #[test]
    fn help_lines_reference_quit_and_buffers() {
        let rendered = help_lines()
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("q / Ctrl+C"));
        assert!(rendered.contains("Space"));
        assert!(!rendered.is_empty());
    }

    #[test]
    fn help_lines_document_the_new_bindings() {
        let rendered = help_lines()
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Reply modal.
        assert!(rendered.contains('r'));
        assert!(rendered.contains("reply to the selected notification"));
        assert!(rendered.contains("send the reply draft"));
        // Esc priority now mentions the reply modal first.
        assert!(rendered.contains("reply modal, help overlay, then detail panel"));
    }

    #[test]
    fn transfers_tab_renders_name_bar_and_percent() {
        use crate::model::TransferEntry;
        let mut state = State::new();
        state.tab = Tab::Transfers;
        state.transfers.insert(
            "t-123456789".to_owned(),
            TransferEntry::from_added("t-123456789", "holiday photo.jpg", 100),
        );

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 8))
            .expect("test backend");
        terminal.draw(|frame| draw(frame, &state)).expect("draw");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("holiday photo.jpg"));
        assert!(screen.contains('░'));
        assert!(screen.contains('%'));
        assert!(!state.help_overlay);
    }

    #[test]
    fn finished_transfer_shows_full_bar_and_100_percent() {
        use crate::model::TransferEntry;
        let mut state = State::new();
        state.tab = Tab::Transfers;
        let mut entry = TransferEntry::from_added("t1", "done.bin", 10);
        entry.apply_progress(10, 10);
        assert!(entry.is_finished());
        state.transfers.insert(entry.id.clone(), entry);

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 8))
            .expect("test backend");
        terminal.draw(|frame| draw(frame, &state)).expect("draw");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("100%"));
        assert!(!screen.contains("░"));
    }

    #[test]
    fn reply_modal_draws_draft_and_subject() {
        use crate::model::NotifRow;
        let mut state = State::new();
        state.notifications.push_back(NotifRow {
            id: "n1".to_owned(),
            app: "kmail".to_owned(),
            title: "Meeting".to_owned(),
            body: String::new(),
        });
        state.reply_open = true;
        state.reply_target = Some("n1".to_owned());
        state.reply_draft = "see you".to_owned();

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24))
            .expect("test backend");
        terminal.draw(|frame| draw(frame, &state)).expect("draw");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("Reply — kmail: Meeting"));
        assert!(screen.contains("> see you_"));
        assert!(screen.contains("Enter send · Esc cancel"));
    }
}
