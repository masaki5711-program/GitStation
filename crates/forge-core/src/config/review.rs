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
//! `review.check_run_retention_days` (default 180, 30 ..= 3650): the worker
//! deletes check runs (with their annotations) last updated more than this
//! many days ago, except the current head of an open merge request
//! (forge-db `checks::expire_old_runs`).
//!
//! `review.worker_job_concurrency` (1 ..= 16; default 4, but at most half
//! of `database.write_max_connections`): how many jobs `forge worker`'s
//! main job runner runs at once (merge load test 2026-11-17, F2). Each
//! kind has its own cap besides (forge-cli `worker::jobs::KIND_CAPS`). A
//! set value above half the write pool is `FG-SYS-0002`
//! (`reason=above_half_write_pool`): every other worker task needs the
//! pool too.
//!
//! `review.path_filter` (default `true`; `true` or `false`): a landing
//! re-checks only the open merge requests whose changed paths may interact
//! with what landed, and carries the result of the others forward to the
//! new target tip (plan 18 S4, ADR-0037 (c)). `false` is the kill switch:
//! no path set is computed, stored or sent, and every landing re-checks
//! every open merge request of the target, as before S4.
//!
//! `review.paths_max` (default 5000, 1 ..= 20000): the most files (and
//! directories) a changed-path set may list. A larger change has an
//! incomplete set and is always checked again.
//!
//! A value outside its range is `FG-SYS-0002` at load time.

use forge_errors::ForgeError;

use super::{invalid, unsigned};

const FANOUT_KEY: &str = "review.mergeability_fanout_max";
const RETENTION_KEY: &str = "review.check_run_retention_days";
const CONCURRENCY_KEY: &str = "review.worker_job_concurrency";
const PATH_FILTER_KEY: &str = "review.path_filter";
const PATHS_MAX_KEY: &str = "review.paths_max";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewConfig {
    /// `review.mergeability_fanout_max`.
    pub mergeability_fanout_max: u32,
    /// `review.check_run_retention_days`.
    pub check_run_retention_days: u32,
    /// `review.worker_job_concurrency` (resolved by [`Self::load`] only:
    /// the default is cut to half the write pool and a set value above it
    /// is refused there; `Default` and a hand-built value are not checked
    /// against any pool).
    pub worker_job_concurrency: u32,
    /// `review.path_filter` (`false`: the kill switch of the path-aware
    /// mergeability).
    pub path_filter: bool,
    /// `review.paths_max`.
    pub paths_max: u32,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            mergeability_fanout_max: Self::DEFAULT_MERGEABILITY_FANOUT_MAX,
            check_run_retention_days: Self::DEFAULT_CHECK_RUN_RETENTION_DAYS,
            worker_job_concurrency: Self::DEFAULT_WORKER_JOB_CONCURRENCY,
            path_filter: Self::DEFAULT_PATH_FILTER,
            paths_max: Self::DEFAULT_PATHS_MAX,
        }
    }
}

impl ReviewConfig {
    pub const DEFAULT_MERGEABILITY_FANOUT_MAX: u32 = 50;
    pub const MIN_MERGEABILITY_FANOUT_MAX: u32 = 1;
    pub const MAX_MERGEABILITY_FANOUT_MAX: u32 = 500;
    pub const DEFAULT_CHECK_RUN_RETENTION_DAYS: u32 = 180;
    pub const MIN_CHECK_RUN_RETENTION_DAYS: u32 = 30;
    pub const MAX_CHECK_RUN_RETENTION_DAYS: u32 = 3650;
    pub const DEFAULT_WORKER_JOB_CONCURRENCY: u32 = 4;
    pub const MAX_WORKER_JOB_CONCURRENCY: u32 = 16;
    pub const DEFAULT_PATH_FILTER: bool = true;
    pub const DEFAULT_PATHS_MAX: u32 = 5000;
    pub const MIN_PATHS_MAX: u32 = 1;
    /// The table's constraint and `merge_paths::PATHS_MAX_LIMIT`.
    pub const MAX_PATHS_MAX: u32 = 20_000;

    /// The cap of a changed-path set when the path filter is on; `None`
    /// when it is off (nothing is computed, stored or sent).
    pub fn path_filter_max(&self) -> Option<usize> {
        self.path_filter
            .then(|| usize::try_from(self.paths_max).unwrap_or(usize::MAX))
    }

    /// `write_max_connections`: `database.write_max_connections` (the
    /// worker's job runs share that pool with its other tasks).
    pub(super) fn load<'a>(
        get: impl Fn(&str) -> Option<&'a str>,
        write_max_connections: u32,
    ) -> Result<Self, ForgeError> {
        let fanout = unsigned(&get, FANOUT_KEY, Self::DEFAULT_MERGEABILITY_FANOUT_MAX)?;
        if fanout < Self::MIN_MERGEABILITY_FANOUT_MAX {
            return Err(invalid(FANOUT_KEY, "below_min"));
        }
        if fanout > Self::MAX_MERGEABILITY_FANOUT_MAX {
            return Err(invalid(FANOUT_KEY, "above_max"));
        }
        let days = unsigned(&get, RETENTION_KEY, Self::DEFAULT_CHECK_RUN_RETENTION_DAYS)?;
        if days < Self::MIN_CHECK_RUN_RETENTION_DAYS {
            return Err(invalid(RETENTION_KEY, "below_min"));
        }
        if days > Self::MAX_CHECK_RUN_RETENTION_DAYS {
            return Err(invalid(RETENTION_KEY, "above_max"));
        }
        let half_pool = (write_max_connections / 2).max(1);
        let concurrency = match get(CONCURRENCY_KEY) {
            None => Self::DEFAULT_WORKER_JOB_CONCURRENCY.min(half_pool),
            Some(_) => {
                let n = unsigned(&get, CONCURRENCY_KEY, Self::DEFAULT_WORKER_JOB_CONCURRENCY)?;
                if n < 1 {
                    return Err(invalid(CONCURRENCY_KEY, "below_min"));
                }
                if n > Self::MAX_WORKER_JOB_CONCURRENCY {
                    return Err(invalid(CONCURRENCY_KEY, "above_max"));
                }
                if n > half_pool {
                    return Err(invalid(CONCURRENCY_KEY, "above_half_write_pool"));
                }
                n
            }
        };
        let path_filter = match get(PATH_FILTER_KEY) {
            None => Self::DEFAULT_PATH_FILTER,
            Some("true") => true,
            Some("false") => false,
            Some(_) => return Err(invalid(PATH_FILTER_KEY, "invalid")),
        };
        let paths_max = unsigned(&get, PATHS_MAX_KEY, Self::DEFAULT_PATHS_MAX)?;
        if paths_max < Self::MIN_PATHS_MAX {
            return Err(invalid(PATHS_MAX_KEY, "below_min"));
        }
        if paths_max > Self::MAX_PATHS_MAX {
            return Err(invalid(PATHS_MAX_KEY, "above_max"));
        }
        Ok(Self {
            mergeability_fanout_max: fanout,
            check_run_retention_days: days,
            worker_job_concurrency: concurrency,
            path_filter,
            paths_max,
        })
    }
}
