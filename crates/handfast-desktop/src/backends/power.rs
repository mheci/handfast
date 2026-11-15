//! Host battery state through sysfs.

use std::io;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::PowerState;
use crate::traits::Power;

/// Reads the first battery under a power-supply root.
pub struct SysfsPower {
    root: PathBuf,
}

impl SysfsPower {
    /// Build a battery adapter over `/sys/class/power_supply`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: PathBuf::from("/sys/class/power_supply"),
        }
    }

    /// Build a battery adapter over a custom root.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Default for SysfsPower {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Power for SysfsPower {
    async fn state(&self) -> Result<Option<PowerState>> {
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(DesktopError::Backend(format!(
                    "failed to read {}: {error}",
                    self.root.display()
                )));
            }
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => return Ok(None),
                Err(error) => {
                    return Err(DesktopError::Backend(format!(
                        "failed to read {}: {error}",
                        self.root.display()
                    )));
                }
            };
            let path = entry.path();
            let kind = match tokio::fs::read_to_string(path.join("type")).await {
                Ok(kind) => kind,
                Err(_) => continue,
            };
            if kind.trim() != "Battery" {
                continue;
            }
            let percent = match tokio::fs::read_to_string(path.join("capacity")).await {
                Ok(capacity) => match capacity.trim().parse::<u8>() {
                    Ok(percent) => percent.min(100),
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            let status = tokio::fs::read_to_string(path.join("status"))
                .await
                .unwrap_or_default();
            let charging = matches!(status.trim(), "Charging" | "Full");
            return Ok(Some(PowerState { percent, charging }));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn battery(root: &Path, capacity: &str, status: &str) {
        let dir = root.join("BAT0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("type"), "Battery\n").unwrap();
        fs::write(dir.join("capacity"), format!("{capacity}\n")).unwrap();
        fs::write(dir.join("status"), format!("{status}\n")).unwrap();
    }

    #[tokio::test]
    async fn a_charging_battery_is_read() {
        let root = tempfile::tempdir().unwrap();
        battery(root.path(), "42", "Charging");
        let power = SysfsPower::with_root(root.path());

        assert_eq!(
            power.state().await.unwrap(),
            Some(PowerState {
                percent: 42,
                charging: true
            })
        );
    }

    #[tokio::test]
    async fn a_full_battery_counts_as_charging() {
        let root = tempfile::tempdir().unwrap();
        battery(root.path(), "100", "Full");
        let power = SysfsPower::with_root(root.path());

        assert_eq!(
            power.state().await.unwrap(),
            Some(PowerState {
                percent: 100,
                charging: true
            })
        );
    }

    #[tokio::test]
    async fn a_discharging_battery_is_not_charging() {
        let root = tempfile::tempdir().unwrap();
        battery(root.path(), "37", "Discharging");
        let power = SysfsPower::with_root(root.path());

        assert_eq!(
            power.state().await.unwrap(),
            Some(PowerState {
                percent: 37,
                charging: false
            })
        );
    }

    #[tokio::test]
    async fn no_battery_is_none() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("AC")).unwrap();
        fs::write(root.path().join("AC").join("type"), "Mains\n").unwrap();
        let power = SysfsPower::with_root(root.path());

        assert_eq!(power.state().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_missing_root_is_none() {
        let root = tempfile::tempdir().unwrap();
        let power = SysfsPower::with_root(root.path().join("missing"));

        assert_eq!(power.state().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_malformed_capacity_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        battery(root.path(), "not-a-number", "Charging");
        let power = SysfsPower::with_root(root.path());

        assert_eq!(power.state().await.unwrap(), None);
    }

    #[tokio::test]
    async fn capacity_is_capped_at_one_hundred() {
        let root = tempfile::tempdir().unwrap();
        battery(root.path(), "150", "Discharging");
        let power = SysfsPower::with_root(root.path());

        assert_eq!(power.state().await.unwrap().unwrap().percent, 100);
    }
}
