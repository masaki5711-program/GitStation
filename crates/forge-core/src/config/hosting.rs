//! `[hosting]`: how long a deleted project stays restorable (D5 MR3,
//! ADR-0015 Amendment D5) and how often the purge looks for projects whose
//! grace period is over (D5 MR4).
//!
//! `hosting.deletion_grace_days` (default 7, at least 1, at most 365): a
//! deleted project is pending deletion for this long; `purge_after` is the
//! deletion time plus it. Restore is possible until the purge job starts.
//!
//! `hosting.purge_interval_secs` (default 300, at least 10, at most 86 400):
//! the period of the `forge worker` purge task. A purge starts at most this
//! long after `purge_after`; a failed purge is retried on the next run.
//!
//! A value outside its range is `FG-SYS-0002` at load time.

use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, unsigned};

const KEY: &str = "hosting.deletion_grace_days";
const PURGE_INTERVAL_KEY: &str = "hosting.purge_interval_secs";
const SECONDS_PER_DAY: u64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostingConfig {
    /// `hosting.deletion_grace_days`, as a duration.
    pub deletion_grace: Duration,
    /// `hosting.purge_interval_secs`, as a duration.
    pub purge_interval: Duration,
}

impl HostingConfig {
    pub const DEFAULT_DELETION_GRACE_DAYS: u32 = 7;
    pub const MIN_DELETION_GRACE_DAYS: u32 = 1;
    pub const MAX_DELETION_GRACE_DAYS: u32 = 365;

    /// The default grace period (7 days).
    pub const DEFAULT_DELETION_GRACE: Duration =
        Duration::from_secs(Self::DEFAULT_DELETION_GRACE_DAYS as u64 * SECONDS_PER_DAY);

    pub const DEFAULT_PURGE_INTERVAL_SECS: u32 = 300;
    pub const MIN_PURGE_INTERVAL_SECS: u32 = 10;
    pub const MAX_PURGE_INTERVAL_SECS: u32 = 86_400;

    /// The default purge period (5 minutes).
    pub const DEFAULT_PURGE_INTERVAL: Duration =
        Duration::from_secs(Self::DEFAULT_PURGE_INTERVAL_SECS as u64);

    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let days = unsigned(&get, KEY, Self::DEFAULT_DELETION_GRACE_DAYS)?;
        if days < Self::MIN_DELETION_GRACE_DAYS {
            return Err(invalid(KEY, "below_min"));
        }
        if days > Self::MAX_DELETION_GRACE_DAYS {
            return Err(invalid(KEY, "above_max"));
        }
        let purge = unsigned(&get, PURGE_INTERVAL_KEY, Self::DEFAULT_PURGE_INTERVAL_SECS)?;
        if purge < Self::MIN_PURGE_INTERVAL_SECS {
            return Err(invalid(PURGE_INTERVAL_KEY, "below_min"));
        }
        if purge > Self::MAX_PURGE_INTERVAL_SECS {
            return Err(invalid(PURGE_INTERVAL_KEY, "above_max"));
        }
        Ok(Self {
            deletion_grace: Duration::from_secs(u64::from(days) * SECONDS_PER_DAY),
            purge_interval: Duration::from_secs(u64::from(purge)),
        })
    }
}
