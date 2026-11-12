//! Process-wide counters of the git subprocesses forge starts (perf
//! diagnosis: how many `git` processes one request costs; each spawn is
//! ~20–40 ms on Windows). Every successful spawn also emits a `debug`
//! event `forge.git.spawn` (target `forge_git::spawn`, with `subcommand`
//! and `kind`), so the spawns of one request can be counted from its
//! `request_id` span in the logs.

use std::sync::atomic::{AtomicU64, Ordering};

static SPAWNS: AtomicU64 = AtomicU64::new(0);

/// git processes started by this process so far (captured, streaming and
/// the long-lived `cat-file --batch-command` processes alike).
pub fn git_spawns() -> u64 {
    SPAWNS.load(Ordering::Relaxed)
}

/// Record one started git process.
pub(crate) fn spawned(subcommand: &'static str, kind: &'static str) {
    SPAWNS.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(
        target: "forge_git::spawn",
        subcommand,
        kind,
        "forge.git.spawn"
    );
}
