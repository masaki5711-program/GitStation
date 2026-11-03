//! `[maintenance]`: the repository maintenance of `forge worker` (M3 O3,
//! plan 11 §5 O3, ADR-0024 "実装で決めたこと（O3）").
//!
//! | key | default | range |
//! |---|---|---|
//! | `maintenance.interval_days` | 7 | 1 ..= 90 — each repository is maintained this often |
//! | `maintenance.window_start` / `window_end` | `02:00` / `05:00` | `HH:MM`, different; the end may be past midnight (`23:00`–`02:00`) |
//! | `maintenance.window_time_zone` | `Asia/Tokyo` | an IANA name, checked by the database when the worker starts |
//! | `maintenance.prune_grace_hours` | 48 | 24 ..= 720 — and longer than the merge reconciliation window plus the merge job's retry budget (checked by the worker) |
//! | `maintenance.step_timeout_secs` | 1800 | 60 ..= [`MaintenanceConfig::MAX_STEP_TIMEOUT_SECS`] — one git step (gc, pack-refs, commit-graph, fsck, counts) |
//! | `maintenance.fsck_interval_days` | 30 | 1 ..= 365 — the full fsck (`--connectivity-only` runs every time) |
//! | `maintenance.batch_max` | 10 | 1 ..= 1000 — repositories enqueued per scheduling pass |
//! | `maintenance.refs_warn` | 100 000 | 1 ..= 10 000 000 — doctor `repositories.refs` warns above it |
//!
//! A value outside its range is `FG-SYS-0002` at load time.

use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, unsigned};

const INTERVAL_KEY: &str = "maintenance.interval_days";
const START_KEY: &str = "maintenance.window_start";
const END_KEY: &str = "maintenance.window_end";
const TZ_KEY: &str = "maintenance.window_time_zone";
const GRACE_KEY: &str = "maintenance.prune_grace_hours";
const STEP_KEY: &str = "maintenance.step_timeout_secs";
const FSCK_KEY: &str = "maintenance.fsck_interval_days";
const BATCH_KEY: &str = "maintenance.batch_max";
const REFS_KEY: &str = "maintenance.refs_warn";

const SECONDS_PER_DAY: u64 = 86_400;

/// The nightly window, as minutes after local midnight (`end` may be
/// smaller than `start`: the window crosses midnight).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceWindow {
    pub start_minute: u16,
    pub end_minute: u16,
}

impl MaintenanceWindow {
    /// Whether `minute` (after local midnight) is inside `[start, end)`.
    pub fn contains(self, minute: u16) -> bool {
        if self.start_minute < self.end_minute {
            (self.start_minute..self.end_minute).contains(&minute)
        } else {
            minute >= self.start_minute || minute < self.end_minute
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceConfig {
    pub interval: Duration,
    pub window: MaintenanceWindow,
    /// An IANA time zone name (the database resolves it).
    pub time_zone: String,
    pub prune_grace: Duration,
    pub step_timeout: Duration,
    pub fsck_interval: Duration,
    pub batch_max: u32,
    pub refs_warn: u64,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(7 * SECONDS_PER_DAY),
            window: MaintenanceWindow {
                start_minute: 2 * 60,
                end_minute: 5 * 60,
            },
            time_zone: "Asia/Tokyo".to_owned(),
            prune_grace: Duration::from_secs(48 * 3600),
            step_timeout: Duration::from_secs(1800),
            fsck_interval: Duration::from_secs(30 * SECONDS_PER_DAY),
            batch_max: 10,
            refs_warn: 100_000,
        }
    }
}

/// `HH:MM` (24 h) as minutes after midnight.
fn parse_hhmm(key: &'static str, text: &str) -> Result<u16, ForgeError> {
    let bad = || invalid(key, "invalid");
    let (h, m) = text.trim().split_once(':').ok_or_else(bad)?;
    if h.len() != 2 || m.len() != 2 {
        return Err(bad());
    }
    let h: u16 = h.parse().map_err(|_| bad())?;
    let m: u16 = m.parse().map_err(|_| bad())?;
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

/// An IANA-shaped name (`Area/Location`, `UTC`): letters, digits, `_`, `-`,
/// `+` and `/` only. Whether the database knows it is checked at start.
fn valid_time_zone(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("//")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-+/".contains(c))
}

fn ranged<'a>(
    get: &impl Fn(&str) -> Option<&'a str>,
    key: &'static str,
    default: u32,
    (min, max): (u32, u32),
) -> Result<u32, ForgeError> {
    let n = unsigned(get, key, default)?;
    if n < min {
        return Err(invalid(key, "below_min"));
    }
    if n > max {
        return Err(invalid(key, "above_max"));
    }
    Ok(n)
}

impl MaintenanceConfig {
    pub const MIN_PRUNE_GRACE_HOURS: u32 = 24;
    pub const MAX_PRUNE_GRACE_HOURS: u32 = 720;
    pub const MIN_STEP_TIMEOUT_SECS: u32 = 60;
    /// One step must fit the maintenance job's run with room for its
    /// ledger writes (the job's timeout is the queue's maximum, 1 h).
    pub const MAX_STEP_TIMEOUT_SECS: u32 = 3000;

    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let d = Self::default();
        let days = |key, default, range| ranged(&get, key, default, range);
        let interval = days(INTERVAL_KEY, 7, (1, 90))?;
        let start = match get(START_KEY) {
            Some(t) => parse_hhmm(START_KEY, t)?,
            None => d.window.start_minute,
        };
        let end = match get(END_KEY) {
            Some(t) => parse_hhmm(END_KEY, t)?,
            None => d.window.end_minute,
        };
        if start == end {
            return Err(invalid(END_KEY, "empty_window"));
        }
        let time_zone = match get(TZ_KEY).map(str::trim) {
            Some(tz) if valid_time_zone(tz) => tz.to_owned(),
            Some(_) => return Err(invalid(TZ_KEY, "invalid")),
            None => d.time_zone,
        };
        let grace = ranged(
            &get,
            GRACE_KEY,
            48,
            (Self::MIN_PRUNE_GRACE_HOURS, Self::MAX_PRUNE_GRACE_HOURS),
        )?;
        let step = ranged(
            &get,
            STEP_KEY,
            1800,
            (Self::MIN_STEP_TIMEOUT_SECS, Self::MAX_STEP_TIMEOUT_SECS),
        )?;
        let fsck = days(FSCK_KEY, 30, (1, 365))?;
        let batch = ranged(&get, BATCH_KEY, 10, (1, 1000))?;
        let refs = ranged(&get, REFS_KEY, 100_000, (1, 10_000_000))?;
        Ok(Self {
            interval: Duration::from_secs(u64::from(interval) * SECONDS_PER_DAY),
            window: MaintenanceWindow {
                start_minute: start,
                end_minute: end,
            },
            time_zone,
            prune_grace: Duration::from_secs(u64::from(grace) * 3600),
            step_timeout: Duration::from_secs(u64::from(step)),
            fsck_interval: Duration::from_secs(u64::from(fsck) * SECONDS_PER_DAY),
            batch_max: batch,
            refs_warn: u64::from(refs),
        })
    }
}
