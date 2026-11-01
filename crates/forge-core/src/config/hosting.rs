//! `[hosting]`: how long a deleted project stays restorable (D5 MR3,
//! ADR-0015 Amendment D5).
//!
//! `hosting.deletion_grace_days` (default 7, at least 1, at most 365): a
//! deleted project is pending deletion for this long; `purge_after` is the
//! deletion time plus it. Restore is possible until the purge job (D5 MR4)
//! starts. A value outside the range is `FG-SYS-0002` at load time.

use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, unsigned};

const KEY: &str = "hosting.deletion_grace_days";
const SECONDS_PER_DAY: u64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostingConfig {
    /// `hosting.deletion_grace_days`, as a duration.
    pub deletion_grace: Duration,
}

impl HostingConfig {
    pub const DEFAULT_DELETION_GRACE_DAYS: u32 = 7;
    pub const MIN_DELETION_GRACE_DAYS: u32 = 1;
    pub const MAX_DELETION_GRACE_DAYS: u32 = 365;

    /// The default grace period (7 days).
    pub const DEFAULT_DELETION_GRACE: Duration =
        Duration::from_secs(Self::DEFAULT_DELETION_GRACE_DAYS as u64 * SECONDS_PER_DAY);

    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let days = unsigned(get, KEY, Self::DEFAULT_DELETION_GRACE_DAYS)?;
        if days < Self::MIN_DELETION_GRACE_DAYS {
            return Err(invalid(KEY, "below_min"));
        }
        if days > Self::MAX_DELETION_GRACE_DAYS {
            return Err(invalid(KEY, "above_max"));
        }
        Ok(Self {
            deletion_grace: Duration::from_secs(u64::from(days) * SECONDS_PER_DAY),
        })
    }
}
