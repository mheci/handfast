//! Host battery reporting.
//!
//! Linux reads `/sys/class/power_supply`. A `BAT*` entry with a readable
//! `capacity` is the primary battery; `status` decides charging. A host with no
//! such entry reports charge `-1`, which is the wire's "no battery" value.
//!
//! The adapter never shells out: the sysfs files are ordinary reads. Windows
//! has no equivalent interface without COM, so [`SysfsBattery`] is only built on
//! Unix; [`UnavailableBattery`] stands in elsewhere and reports no battery.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::traits::{Battery, BatteryStatus};

/// Low-battery threshold, matching the desktop client's 15 percent rule.
pub const LOW_THRESHOLD: i32 = 15;

/// The sysfs power supply root.
pub const SYSFS_ROOT: &str = "/sys/class/power_supply";

/// Reads the host battery from a sysfs-like directory tree.
///
/// The `capacity` and `status` files are read on every call, so a status
/// report is never stale. A missing or unreadable battery reports "no
/// battery" instead of failing the plugin.
#[derive(Debug, Clone)]
pub struct SysfsBattery {
    root: PathBuf,
}

impl SysfsBattery {
    /// Build an adapter over the real sysfs tree.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: PathBuf::from(SYSFS_ROOT),
        }
    }

    /// Build an adapter over a test directory.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The primary battery directory, if one exists.
    fn primary(&self) -> Option<PathBuf> {
        let entries = std::fs::read_dir(&self.root).ok()?;
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("BAT"))
            })
            .map(|entry| entry.path())
            .collect();
        candidates.sort();
        candidates
            .into_iter()
            .find(|path| path.join("capacity").is_file())
    }
}

impl Default for SysfsBattery {
    fn default() -> Self {
        Self::new()
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
}

/// Interpret a `status` file and a `capacity` file.
///
/// A capacity outside 0-100 is treated as unreadable so a driver quirk cannot
/// send an impossible percentage to the peer.
fn interpret(capacity: &str, status: Option<&str>) -> BatteryStatus {
    let charge = capacity
        .parse::<i32>()
        .ok()
        .filter(|charge| (0..=100).contains(charge))
        .unwrap_or(-1);
    let charging = status.is_some_and(|status| {
        matches!(
            status,
            "Charging" | "Full" | "Not charging" | "Unknown" | "Warming"
        )
    });
    let threshold = if charge >= 0 && charge <= LOW_THRESHOLD && !charging {
        1
    } else {
        0
    };
    BatteryStatus {
        charge,
        charging,
        threshold,
    }
}

#[async_trait]
impl Battery for SysfsBattery {
    async fn status(&self) -> Result<BatteryStatus> {
        let Some(primary) = self.primary() else {
            return Ok(BatteryStatus::default());
        };
        let Some(capacity) = read_trimmed(&primary.join("capacity")) else {
            return Ok(BatteryStatus::default());
        };
        let status = read_trimmed(&primary.join("status"));
        Ok(interpret(&capacity, status.as_deref()))
    }
}

/// A battery adapter for hosts that cannot report a battery.
///
/// It answers with the "no battery" value rather than failing, so the battery
/// plugin can still be advertised and answer a peer's request.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableBattery;

#[async_trait]
impl Battery for UnavailableBattery {
    async fn status(&self) -> Result<BatteryStatus> {
        Ok(BatteryStatus::default())
    }
}

/// Build the host battery adapter.
///
/// Linux reads sysfs. Every other platform reports "no battery".
#[must_use]
pub fn adapter() -> Arc<dyn Battery> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(SysfsBattery::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(UnavailableBattery)
    }
}

/// Whether the host can report a real battery.
///
/// Used by the daemon to decide whether to advertise the battery plugin.
#[must_use]
pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    {
        SysfsBattery::new().primary().is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(battery: &str, capacity: Option<&str>, status: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create a temp dir");
        let path = dir.path().join(battery);
        std::fs::create_dir_all(&path).expect("create the battery directory");
        if let Some(capacity) = capacity {
            std::fs::write(path.join("capacity"), capacity).expect("write capacity");
        }
        if let Some(status) = status {
            std::fs::write(path.join("status"), status).expect("write status");
        }
        dir
    }

    #[tokio::test]
    async fn discharging_battery_reports_charge_and_threshold() {
        let dir = tree("BAT0", Some("10\n"), Some("Discharging\n"));
        let battery = SysfsBattery::with_root(dir.path());
        assert_eq!(
            battery.status().await.unwrap(),
            BatteryStatus {
                charge: 10,
                charging: false,
                threshold: 1,
            }
        );
    }

    #[tokio::test]
    async fn charging_battery_suppresses_the_low_threshold() {
        let dir = tree("BAT1", Some("42"), Some("Charging"));
        let battery = SysfsBattery::with_root(dir.path());
        assert_eq!(
            battery.status().await.unwrap(),
            BatteryStatus {
                charge: 42,
                charging: true,
                threshold: 0,
            }
        );
    }

    #[tokio::test]
    async fn no_battery_reports_minus_one() {
        let dir = tempfile::tempdir().expect("create a temp dir");
        let battery = SysfsBattery::with_root(dir.path());
        assert_eq!(battery.status().await.unwrap(), BatteryStatus::default());
    }

    #[tokio::test]
    async fn a_mains_only_entry_is_not_a_battery() {
        let dir = tree("AC", Some("100"), Some("Charging"));
        let battery = SysfsBattery::with_root(dir.path());
        assert_eq!(battery.status().await.unwrap(), BatteryStatus::default());
    }

    #[tokio::test]
    async fn an_impossible_capacity_is_reported_as_no_battery() {
        let dir = tree("BAT0", Some("250"), Some("Discharging"));
        let battery = SysfsBattery::with_root(dir.path());
        assert_eq!(battery.status().await.unwrap().charge, -1);
    }

    #[tokio::test]
    async fn the_unavailable_adapter_never_fails() {
        assert_eq!(
            UnavailableBattery.status().await.unwrap(),
            BatteryStatus::default()
        );
    }
}
