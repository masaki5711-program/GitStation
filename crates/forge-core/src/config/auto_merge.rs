//! `[auto_merge]`: how the auto-merge worker of `forge serve` waits (plan
//! 18 S3, ADR-0037 (a)).
//!
//! `auto_merge.safety_poll_ms` (default 30000; 100 ..= 300000): how often
//! the worker sweeps every waiting merge request by itself. Between two
//! sweeps it decides only the merge requests a sequenced event names
//! (`forge_events_sequenced`) or a mergeability result announces
//! (`forge_auto_merge`); both are hints, so a lost one costs at most this
//! long. While the LISTEN connection of the process is down the worker
//! sweeps every five seconds at most (the period before plan 18).
//!
//! The way back: `auto_merge.safety_poll_ms = 5000` sweeps every five
//! seconds, as before plan 18 S3 — notifications then only make it
//! faster, never slower.
//!
//! A value outside its range is `FG-SYS-0002` at load time.

use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, unsigned};

const SAFETY_POLL_KEY: &str = "auto_merge.safety_poll_ms";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoMergeConfig {
    /// `auto_merge.safety_poll_ms`.
    pub safety_poll_ms: u32,
}

impl Default for AutoMergeConfig {
    fn default() -> Self {
        Self {
            safety_poll_ms: Self::DEFAULT_SAFETY_POLL_MS,
        }
    }
}

impl AutoMergeConfig {
    pub const DEFAULT_SAFETY_POLL_MS: u32 = 30_000;
    pub const MIN_SAFETY_POLL_MS: u32 = 100;
    pub const MAX_SAFETY_POLL_MS: u32 = 300_000;

    /// The period of the worker's own sweep.
    pub fn safety_poll(&self) -> Duration {
        Duration::from_millis(u64::from(self.safety_poll_ms))
    }

    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let safety = unsigned(&get, SAFETY_POLL_KEY, Self::DEFAULT_SAFETY_POLL_MS)?;
        if safety < Self::MIN_SAFETY_POLL_MS {
            return Err(invalid(SAFETY_POLL_KEY, "below_min"));
        }
        if safety > Self::MAX_SAFETY_POLL_MS {
            return Err(invalid(SAFETY_POLL_KEY, "above_max"));
        }
        Ok(Self {
            safety_poll_ms: safety,
        })
    }
}
