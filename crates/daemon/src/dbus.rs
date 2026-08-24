//! D-Bus and platform service listeners feeding the core event bus.
//!
//! Three long-running listeners live here; each is written as a supervised
//! task (an async function returning [`Result`](handfast_core::error::Result))
//! suitable for `supervisor.spawn`:
//!
//! | Task                       | Source                                        | Published events                    |
//! |----------------------------|-----------------------------------------------|-------------------------------------|
//! | [`battery_monitor`]        | `/sys/class/power_supply` (sysfs polling)     | `Event::LogRecord` on level changes |
//! | [`notifications_listener`] | `org.freedesktop.Notifications` (session bus) | `Event::NotificationReceived`       |
//! | [`upower_battery`]         | `org.freedesktop.UPower` (system bus)         | `Event::LogRecord` on level changes |
//!
//! # Resilience contract
//!
//! D-Bus daemons and battery hardware are optional: containers, headless CI
//! boxes, and non-Linux developer machines routinely lack them. Every
//! listener therefore reports a *single* warning (tracing log plus one
//! `Event::LogRecord`) and then parks forever via `std::future::pending`
//! instead of returning `Err`. Returning `Err` would make the supervisor
//! respawn us with exponential backoff — a pointless crash-loop against a
//! dependency that will not appear mid-session. A parked task costs one idle
//! future and is torn down normally during shutdown.

use std::future::pending;
use std::time::Duration;

use futures_util::StreamExt;
use handfast_core::bus::{Bus, Event};
use handfast_core::error::{Error, Result};

/// Logging target shared by every listener in this module.
const TARGET: &str = "handfast::dbus";

/// How often [`battery_monitor`] re-reads sysfs.
const SYSFS_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Kernel sysfs root exposing power-supply devices.
const POWER_SUPPLY_ROOT: &str = "/sys/class/power_supply";

/// Sysfs entries for batteries are named `BAT*` (`BAT0`, `BAT1`, ...).
const BATTERY_NAME_PREFIX: &str = "BAT";

/// Sysfs file holding the charge percentage (0-100).
const CAPACITY_FILE: &str = "capacity";

/// Sysfs file holding the discharge status (`Charging`, `Discharging`, ...).
const STATUS_FILE: &str = "status";

/// Notifications interface watched on the session bus.
const NOTIFICATIONS_INTERFACE: &str = "org.freedesktop.Notifications";

/// Well-known name of the UPower service on the system bus.
const UPOWER_SERVICE: &str = "org.freedesktop.UPower";

/// Object path of the aggregated battery UPower exposes for the whole system.
const UPOWER_DISPLAY_DEVICE_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

/// Device interface implemented by [`UPOWER_DISPLAY_DEVICE_PATH`].
const UPOWER_DEVICE_INTERFACE: &str = "org.freedesktop.UPower.Device";

/// UPower property carrying the charge percentage (`f64`).
const PERCENTAGE_PROPERTY: &str = "Percentage";

/// UPower property carrying the charge state (numeric enum).
const STATE_PROPERTY: &str = "State";

/// Battery level snapshot used for change detection: rounded percentage plus
/// a human-readable charge-state label.
///
/// The percentage is kept as an `i64` so float jitter from UPower (e.g. 87.0
/// drifting to 86.999) does not produce spurious events.
type BatteryLevel = (i64, String);

/// Fallback battery watcher: polls `/sys/class/power_supply` every 30 seconds
/// with synchronous `std::fs` reads and publishes an `Event::LogRecord`
/// whenever the aggregate battery level changes.
///
/// Multiple batteries are averaged into one percentage and their distinct
/// statuses joined (`"discharging+charging"`). If sysfs is missing or holds no
/// batteries the task logs once and parks forever — there is nothing to watch,
/// and retrying would only spam the log. The tiny kernel attribute files do
/// not justify async IO at this cadence.
pub async fn battery_monitor(bus: Bus) -> Result<()> {
    let mut last: Option<BatteryLevel> = match read_sysfs_levels() {
        Ok(levels) if !levels.is_empty() => Some(summarize_levels(&levels)),
        _ => None,
    };
    let Some(initial) = last.clone() else {
        return park_forever(
            &bus,
            "sysfs battery monitor",
            "no readable batteries under /sys/class/power_supply",
        )
        .await;
    };

    tracing::info!(target: TARGET, path = POWER_SUPPLY_ROOT, "polling sysfs batteries");
    bus.publish(Event::LogRecord {
        level: "info".to_string(),
        msg: battery_message(&initial),
    });

    loop {
        tokio::time::sleep(SYSFS_POLL_INTERVAL).await;
        let Ok(levels) = read_sysfs_levels() else {
            // Transient sysfs hiccup; keep the last known level and retry.
            tracing::debug!(target: TARGET, "sysfs read failed this cycle");
            continue;
        };
        if levels.is_empty() {
            continue;
        }
        let snapshot = summarize_levels(&levels);
        if Some(&snapshot) != last.as_ref() {
            bus.publish(Event::LogRecord {
                level: "info".to_string(),
                msg: battery_message(&snapshot),
            });
            last = Some(snapshot);
        }
    }
}

/// Session-bus notification listener (placeholder).
///
/// Watches signals on the `org.freedesktop.Notifications` interface and
/// republishes each one as an [`Event::NotificationReceived`]. This is a
/// stand-in for Phase 3: capturing other applications' `Notify` calls in full
/// requires either implementing the server side of the interface or becoming a
/// privileged bus monitor. Until then the observable signals
/// (`ActionInvoked`, `NotificationClosed`) provide a usable trickle of
/// notification activity for exercising the UI plumbing.
///
/// If the session bus is unavailable the task logs once and parks forever
/// (see the module-level resilience contract).
pub async fn notifications_listener(bus: Bus) -> Result<()> {
    if let Err(err) = run_notifications_listener(&bus).await {
        return park_forever(&bus, "notifications listener", &err).await;
    }
    Ok(())
}

/// System-bus battery watcher backed by UPower.
///
/// Connects to `org.freedesktop.UPower`, subscribes to `PropertiesChanged`
/// for the aggregated `DisplayDevice`, and publishes an `Event::LogRecord`
/// whenever the displayed percentage or charge state changes. The percentage
/// is rounded to whole points so sub-percent drift stays quiet.
///
/// If the system bus or UPower is unavailable the task logs once and parks
/// forever; likewise if the property streams end mid-run (bus went away),
/// rather than letting the supervisor crash-loop against a dead socket.
pub async fn upower_battery(bus: Bus) -> Result<()> {
    if let Err(err) = run_upower_battery(&bus).await {
        return park_forever(&bus, "UPower battery monitor", &err).await;
    }
    Ok(())
}

/// Report a permanent startup failure exactly once, then park forever.
///
/// Never resolves: the trailing `Ok(())` merely satisfies the supervised-task
/// signature (`Future<Output = Result<()>>`). Shutdown tears the parked future
/// down through the supervisor like any other child task.
async fn park_forever(bus: &Bus, service: &str, cause: impl std::fmt::Display) -> Result<()> {
    tracing::warn!(target: TARGET, %cause, "{service}: unavailable, parking until restart");
    bus.publish(Event::LogRecord {
        level: "warn".to_string(),
        msg: format!("dbus: {service} unavailable ({cause}); listener parked"),
    });
    pending::<()>().await;
    Ok(())
}

/// Connect to the session bus and forward Notifications signals onto `bus`.
async fn run_notifications_listener(bus: &Bus) -> Result<()> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|err| Error::Other(format!("session bus unavailable: {err}")))?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(NOTIFICATIONS_INTERFACE)
        .map_err(|err| Error::Other(format!("invalid match rule: {err}")))?
        .build();
    let mut signals = zbus::MessageStream::for_match_rule(rule, &conn, None)
        .await
        .map_err(|err| Error::Other(format!("cannot watch Notifications signals: {err}")))?;

    tracing::info!(target: TARGET, "watching {NOTIFICATIONS_INTERFACE} signals");
    while let Some(item) = signals.next().await {
        match item {
            Ok(message) => publish_notification_signal(bus, &message),
            Err(err) => {
                tracing::debug!(target: TARGET, %err, "Notifications signal stream hiccup")
            }
        }
    }
    Err(Error::Other(
        "Notifications signal stream ended".to_string(),
    ))
}

/// Map one Notifications-interface signal onto an
/// [`Event::NotificationReceived`].
///
/// Field extraction is best-effort: `ActionInvoked` bodies decode as
/// `(u32, String)` and `NotificationClosed` as `(u32, u32)`; anything else
/// falls back to the signal member name for identification.
fn publish_notification_signal(bus: &Bus, message: &zbus::Message) {
    let header = message.header();
    let member = header
        .member()
        .map(|name| name.to_string())
        .unwrap_or_else(|| "Unknown".to_string());
    let sender = header
        .sender()
        .map(|name| name.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let body = message.body();
    let (id, detail) = if let Ok((id, action)) = body.deserialize::<(u32, String)>() {
        (Some(id.to_string()), action)
    } else if let Ok((id, reason)) = body.deserialize::<(u32, u32)>() {
        (Some(id.to_string()), reason.to_string())
    } else {
        (None, String::new())
    };

    tracing::debug!(target: TARGET, %member, %sender, "notification signal received");
    bus.publish(Event::NotificationReceived {
        id: id.unwrap_or_else(|| member.clone()),
        app: sender,
        title: member,
        body: detail,
    });
}

/// Connect to the system bus, seed the initial level, and stream changes.
async fn run_upower_battery(bus: &Bus) -> Result<()> {
    let conn = zbus::Connection::system()
        .await
        .map_err(|err| Error::Other(format!("system bus unavailable: {err}")))?;
    let proxy = zbus::Proxy::new(
        &conn,
        UPOWER_SERVICE,
        UPOWER_DISPLAY_DEVICE_PATH,
        UPOWER_DEVICE_INTERFACE,
    )
    .await
    .map_err(|err| Error::Other(format!("UPower DisplayDevice proxy: {err}")))?;

    // Publish whatever UPower currently reports so UI surfaces bootstrap even
    // before the first hardware event arrives.
    let mut last: Option<BatteryLevel> = match read_upower_snapshot(&proxy).await {
        Ok(snapshot) => {
            bus.publish(Event::LogRecord {
                level: "info".to_string(),
                msg: battery_message(&snapshot),
            });
            Some(snapshot)
        }
        Err(err) => {
            tracing::debug!(target: TARGET, %err, "initial UPower read failed; waiting for events");
            None
        }
    };

    let percentage_changes = proxy
        .receive_property_changed::<f64>(PERCENTAGE_PROPERTY)
        .await;
    let state_changes = proxy.receive_property_changed::<u32>(STATE_PROPERTY).await;
    tokio::pin!(percentage_changes, state_changes);

    loop {
        tokio::select! {
            changed = percentage_changes.next() => {
                if changed.is_none() {
                    break;
                }
            }
            changed = state_changes.next() => {
                if changed.is_none() {
                    break;
                }
            }
        }
        match read_upower_snapshot(&proxy).await {
            Ok(snapshot) if Some(&snapshot) != last.as_ref() => {
                bus.publish(Event::LogRecord {
                    level: "info".to_string(),
                    msg: battery_message(&snapshot),
                });
                last = Some(snapshot);
            }
            Ok(_) => {}
            Err(err) => {
                tracing::debug!(target: TARGET, %err, "UPower refresh failed; keeping previous level")
            }
        }
    }
    Err(Error::Other("UPower property stream ended".to_string()))
}

/// Read percentage + state from the UPower DisplayDevice proxy.
async fn read_upower_snapshot(proxy: &zbus::Proxy<'_>) -> zbus::Result<BatteryLevel> {
    let percentage: f64 = proxy.get_property(PERCENTAGE_PROPERTY).await?;
    let state: u32 = proxy.get_property(STATE_PROPERTY).await?;
    Ok((
        (percentage.round().clamp(0.0, 100.0)) as i64,
        upower_state_label(state).to_string(),
    ))
}

/// Human-readable label for the numeric `org.freedesktop.UPower.Device.State`
/// enum values.
fn upower_state_label(state: u32) -> &'static str {
    match state {
        1 => "charging",
        2 => "discharging",
        3 => "empty",
        4 => "fully charged",
        5 => "pending charge",
        6 => "pending discharge",
        _ => "unknown",
    }
}

/// Scan every `BAT*` device under [`POWER_SUPPLY_ROOT`], returning its
/// capacity percentage and lowercased status. Entries that cannot be read or
/// parsed are skipped silently.
fn read_sysfs_levels() -> std::io::Result<Vec<(i64, String)>> {
    let mut levels = Vec::new();
    for entry in std::fs::read_dir(POWER_SUPPLY_ROOT)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(BATTERY_NAME_PREFIX)
        {
            continue;
        }
        let path = entry.path();
        let Ok(capacity_raw) = std::fs::read_to_string(path.join(CAPACITY_FILE)) else {
            continue;
        };
        let Ok(capacity) = capacity_raw.trim().parse::<i64>() else {
            continue;
        };
        let status = std::fs::read_to_string(path.join(STATUS_FILE))
            .map(|text| text.trim().to_lowercase())
            .unwrap_or_default();
        let status = if status.is_empty() {
            "unknown".to_string()
        } else {
            status
        };
        levels.push((capacity, status));
    }
    Ok(levels)
}

/// Collapse per-battery levels into one aggregate snapshot.
fn summarize_levels(levels: &[(i64, String)]) -> BatteryLevel {
    let total: i64 = levels.iter().map(|(capacity, _)| *capacity).sum();
    let average = total / levels.len().max(1) as i64;

    let mut states: Vec<&str> = Vec::new();
    for (_, state) in levels {
        if !states.contains(&state.as_str()) {
            states.push(state.as_str());
        }
    }
    (average.clamp(0, 100), states.join("+"))
}

/// Render a battery snapshot as a structured log message.
fn battery_message(level: &BatteryLevel) -> String {
    format!("battery: {}% ({})", level.0, level.1)
}
