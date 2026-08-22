//! View construction for the Handfast GUI.
//!
//! Pure functions over [`HandfastApp`] producing rock-solid built-in widgets
//! only (text, button, checkbox, column, row, container, scrollable, rule,
//! progress bar, text input). No display-server specifics live here.

use iced::widget::{
    button, checkbox, column, container, horizontal_space, progress_bar, row, rule, scrollable,
    text, text_input,
};
use iced::{Alignment, Element, Length};

use crate::app::{HandfastApp, Message};
use crate::model::{ConnState, DeviceCard, Tab};

/// Root layout: banner, tabbed work area, status footer.
pub(crate) fn root(app: &HandfastApp) -> Element<'_, Message> {
    column![
        banner_bar(app),
        rule::horizontal(2),
        work_area(app),
        rule::horizontal(2),
        footer(app),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// Dismissible error banner, or nothing while things are calm.
fn banner_bar(app: &HandfastApp) -> Element<'_, Message> {
    match &app.banner {
        Some(banner_text) => container(
            row![
                text(format!("! {banner_text}")).width(Length::Fill),
                button("dismiss").on_press(Message::BannerClosed),
            ]
            .spacing(8),
        )
        .width(Length::Fill)
        .padding(6.0)
        .into(),
        None => column![].into(),
    }
}

/// Sidebar plus per-tab main pane.
fn work_area(app: &HandfastApp) -> Element<'_, Message> {
    row![sidebar(app), rule::vertical(2), pane(app)]
        .width(Length::Fill)
        .height(Length::Fill)
        .spacing(8)
        .padding(8.0)
        .into()
}

/// Navigation tabs, connection status and the clipboard sender.
fn sidebar(app: &HandfastApp) -> Element<'_, Message> {
    let mut tabs = column![].spacing(4);
    for tab in Tab::ALL {
        let active = app.tab == tab;
        // The active tab is rendered inert (no press handler), which both
        // communicates the selection and disables redundant clicks.
        let mut tab_button = button(text(tab.label()).width(Length::Fill)).width(Length::Fill);
        if !active {
            tab_button = tab_button.on_press(Message::TabSelected(tab));
        }
        tabs = tabs.push(tab_button);
    }

    let can_refresh = app.conn == ConnState::Connected;
    column![
        tabs,
        rule::horizontal(2),
        text(format!("ipc: {}", app.conn.label())).size(12),
        text(daemon_label(app)).size(12),
        rule::horizontal(2),
        clipboard_panel(app),
        button("refresh devices").on_press_maybe(can_refresh.then_some(Message::RefreshPressed)),
    ]
    .width(Length::Fixed(240.0))
    .spacing(8)
    .into()
}

/// Clipboard-to-daemon sender panel.
fn clipboard_panel(app: &HandfastApp) -> Element<'_, Message> {
    let can_send = app.conn == ConnState::Connected && !app.clipboard_draft.trim().is_empty();

    let mut panel = column![
        text("clipboard").size(12),
        text_input("text to send", &app.clipboard_draft)
            .on_input(Message::ClipboardChanged)
            .on_submit(|_submitted| Message::SendPressed)
            .padding(4.0)
            .width(Length::Fill),
        button("send").on_press_maybe(can_send.then_some(Message::SendPressed)),
    ]
    .spacing(4);

    if let Some(status) = &app.clipboard_status {
        panel = panel.push(text(status.clone()).size(11));
    }
    panel.into()
}

/// Main pane body for the active tab.
fn pane(app: &HandfastApp) -> Element<'_, Message> {
    match app.tab {
        Tab::Devices => devices_pane(app),
        Tab::Transfers => transfers_pane(app),
        Tab::Notifications => notifications_pane(app),
        Tab::Logs => logs_pane(app),
    }
}

/// Scrollable device card list with an inline detail panel below it.
fn devices_pane(app: &HandfastApp) -> Element<'_, Message> {
    if app.devices.is_empty() {
        let hint = match app.conn {
            ConnState::Connected => "discovering devices...",
            _ => "waiting for the daemon...",
        };
        return text(hint).size(12).into();
    }

    let mut cards = column![].spacing(4);
    for (index, device) in app.devices.iter().enumerate() {
        cards = cards.push(device_row(app, index, device));
    }

    let list: Element<'_, Message> = scrollable(cards)
        .height(Length::Fill)
        .width(Length::Fill)
        .into();

    match selected_card(app) {
        Some(device) => column![list, rule::horizontal(2), detail_panel(app, device)]
            .width(Length::Fill)
            .height(Length::Fill)
            .spacing(4)
            .into(),
        None => list,
    }
}

/// One clickable device card carrying its pair/unpair action.
fn device_row(app: &HandfastApp, index: usize, device: &DeviceCard) -> Element<'_, Message> {
    let action = if device.state == "pairing" || device.state == "unpairing" {
        // In-flight request: render an inert placeholder button.
        button(text(device.state.clone()).size(11))
    } else if device.paired {
        button("unpair").on_press(Message::UnpairPressed(device.id.clone()))
    } else {
        button("pair").on_press(Message::PairPressed(device.id.clone()))
    };

    let marker = if app.selected == Some(index) {
        "> "
    } else {
        ""
    };
    let info = column![
        text(format!("{marker}{}", device.name)),
        text(format!("{}/{}", device.kind, device.state)).size(11),
    ]
    .spacing(2);

    button(
        row![info, horizontal_space(), action]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .on_press(Message::DeviceSelected(index))
    .into()
}

/// Plugin toggles and the file-transfer sender for the selected device.
fn detail_panel(app: &HandfastApp, device: &DeviceCard) -> Element<'_, Message> {
    let mut panel = column![
        text(format!("selected: {} ({})", device.name, device.id)).size(12),
        rule::horizontal(2),
        text("plugins").size(12),
    ]
    .spacing(6);

    if app.plugins.is_empty() {
        let hint = match app.conn {
            ConnState::Connected => "no plugins reported",
            _ => "connect to list plugins",
        };
        panel = panel.push(text(hint).size(11));
    } else {
        let mut rows = column![].spacing(2);
        for plugin in &app.plugins {
            let name = plugin.name.clone();
            rows = rows.push(
                checkbox(plugin.title.clone(), plugin.enabled)
                    .on_toggle(move |checked| Message::TogglePlugin(name.clone(), checked)),
            );
        }
        panel = panel.push(rows);
    }

    panel = panel.push(rule::horizontal(2));
    panel = panel.push(
        row![
            text_input("/absolute/path/file.bin", &app.file_path_draft)
                .on_input(Message::PathChanged)
                .on_submit(|_submitted| Message::QueueFilePressed)
                .width(Length::Fill),
            button("send file").on_press(Message::QueueFilePressed),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    );

    panel.width(Length::Fill).padding(4.0).into()
}

/// Transfer progress rows.
fn transfers_pane(app: &HandfastApp) -> Element<'_, Message> {
    if app.transfers.is_empty() {
        return text("no transfers yet").size(12).into();
    }

    let mut rows = column![].spacing(4);
    for transfer in &app.transfers {
        rows = rows.push(
            row![
                text(transfer.id.clone())
                    .size(11)
                    .width(Length::Fixed(160.0)),
                progress_bar(0.0..=100.0, transfer.percent()),
                text(format!("{}/{} B", transfer.done, transfer.total)).size(11),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }
    scrollable(rows.padding(4.0))
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
}

/// Notification rows with per-row dismissal.
fn notifications_pane(app: &HandfastApp) -> Element<'_, Message> {
    if app.notifications.is_empty() {
        return text("no notifications").size(12).into();
    }

    let mut rows = column![].spacing(4);
    for notification in &app.notifications {
        let id = notification.id.clone();
        rows = rows.push(
            row![
                column![
                    text(format!("{}: {}", notification.app, notification.title)).size(12),
                    text(notification.body.clone()).size(11),
                ]
                .spacing(2)
                .width(Length::Fill),
                button("dismiss").on_press(Message::DismissPressed(id)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }
    scrollable(rows.padding(4.0))
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
}

/// Daemon-forwarded log lines.
fn logs_pane(app: &HandfastApp) -> Element<'_, Message> {
    if app.logs.is_empty() {
        return text("no log entries").size(12).into();
    }

    let mut lines = column![].spacing(1);
    for line in &app.logs {
        lines = lines.push(text(line.clone()).size(11));
    }
    scrollable(lines.padding(4.0))
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
}

/// Status footer: IPC state, daemon identity, GUI version.
fn footer(app: &HandfastApp) -> Element<'_, Message> {
    row![
        text(format!("ipc: {}", app.conn.label())).size(11),
        horizontal_space(),
        text(daemon_label(app)).size(11),
        horizontal_space(),
        text(concat!("handfast-gui v", env!("CARGO_PKG_VERSION"))).size(11),
    ]
    .width(Length::Fill)
    .padding([2, 8])
    .into()
}

/// Selected device lookup shared by the devices pane.
fn selected_card(app: &HandfastApp) -> Option<&DeviceCard> {
    app.selected.and_then(|index| app.devices.get(index))
}

/// Human-readable daemon identity line.
fn daemon_label(app: &HandfastApp) -> String {
    match &app.daemon_version {
        Some(version) => format!("daemon ipc v{version}"),
        None => "daemon: unknown".to_owned(),
    }
}
