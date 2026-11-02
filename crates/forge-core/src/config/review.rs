//! `[review]`: merge requests (M3 R2, ADR-0024 §3).
//!
//! `review.mergeability_fanout_max` (default 50, at least 1, at most 500):
//! one push to a target branch enqueues the mergeability check for at most
//! this many open merge requests targeting it (the most recently updated
//! first). The others are marked `unchecked` (their merge stays blocked on
//! `mergeability_unknown`, never "mergeable") and the overflow is logged
//! once (`forge.review.mergeability_deferred`); their next push, retarget
//! or reopen checks them again. 500 matches the version job's own fan-out
//! cap (`MAX_REFRESH_FANOUT`).
//!
//! A value outside its range is `FG-SYS-0002` at load time.

use forge_errors::ForgeError;

use super::{invalid, unsigned};

const FANOUT_KEY: &str = "review.mergeability_fanout_max";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewConfig {
    /// `review.mergeability_fanout_max`.
    pub mergeability_fanout_max: u32,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            mergeability_fanout_max: Self::DEFAULT_MERGEABILITY_FANOUT_MAX,
        }
    }
}

impl ReviewConfig {
    pub const DEFAULT_MERGEABILITY_FANOUT_MAX: u32 = 50;
    pub const MIN_MERGEABILITY_FANOUT_MAX: u32 = 1;
    pub const MAX_MERGEABILITY_FANOUT_MAX: u32 = 500;

    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let fanout = unsigned(&get, FANOUT_KEY, Self::DEFAULT_MERGEABILITY_FANOUT_MAX)?;
        if fanout < Self::MIN_MERGEABILITY_FANOUT_MAX {
            return Err(invalid(FANOUT_KEY, "below_min"));
        }
        if fanout > Self::MAX_MERGEABILITY_FANOUT_MAX {
            return Err(invalid(FANOUT_KEY, "above_max"));
        }
        Ok(Self {
            mergeability_fanout_max: fanout,
        })
    }
}
