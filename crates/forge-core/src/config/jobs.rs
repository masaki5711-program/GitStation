//! `[jobs]`: how the job runners of `forge worker` wait for work (plan 18
//! S2, ADR-0037 (a)).
//!
//! `jobs.safety_poll_ms` (default 10000; 100 ..= 60000): the safety-net
//! period of an idle job runner. A runner is woken by the `forge_jobs`
//! notification an enqueue sends (forge-db `wake::WakeHub`); the
//! notification is a hint, so the runner still looks by itself at least
//! this often — a lost notification costs at most this long. While the
//! LISTEN connection of the process is down, an idle runner looks every
//! second at most (the poll period before plan 18), whatever this says.
//!
//! The way back: `jobs.safety_poll_ms = 1000` makes an idle runner look
//! every second, as it did before plan 18 S2 — notifications then only
//! make it faster, never slower.
//!
//! A value outside its range is `FG-SYS-0002` at load time.

use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, unsigned};

const SAFETY_POLL_KEY: &str = "jobs.safety_poll_ms";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobsConfig {
    /// `jobs.safety_poll_ms`.
    pub safety_poll_ms: u32,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            safety_poll_ms: Self::DEFAULT_SAFETY_POLL_MS,
        }
    }
}

impl JobsConfig {
    pub const DEFAULT_SAFETY_POLL_MS: u32 = 10_000;
    pub const MIN_SAFETY_POLL_MS: u32 = 100;
    pub const MAX_SAFETY_POLL_MS: u32 = 60_000;

    /// The safety-net period of an idle job runner.
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
