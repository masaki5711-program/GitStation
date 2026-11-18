//! Process-wide counters of the git subprocesses forge starts (perf
//! diagnosis: how many `git` processes one request costs; each spawn is
//! ~20–40 ms on Windows). Every successful spawn also emits a `debug`
//! event `forge.git.spawn` (target `forge_git::spawn`, with `subcommand`
//! and `kind`), so the spawns of one request can be counted from its
//! `request_id` span in the logs.
//!
//! [`counting_spawns`] counts the spawns of one piece of work apart from
//! everything else the process does at the same time (plan 18 S0: the git
//! processes one merge-queue batch costs, while requests are served).

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

static SPAWNS: AtomicU64 = AtomicU64::new(0);

tokio::task_local! {
    /// The spawns of the [`counting_spawns`] scope this task is in.
    static SCOPED: AtomicU64;
}

/// git processes started by this process so far (captured, streaming and
/// the long-lived `cat-file --batch-command` processes alike).
pub fn git_spawns() -> u64 {
    SPAWNS.load(Ordering::Relaxed)
}

/// Run `work`, counting the git processes it starts on this task
/// ([`scoped_git_spawns`] reads the count from inside). A count only: no
/// effect on the work or on [`git_spawns`]. Work moved to another task
/// (`tokio::spawn`) is not counted.
pub async fn counting_spawns<F: Future>(work: F) -> F::Output {
    SCOPED.scope(AtomicU64::new(0), work).await
}

/// git processes this task started so far inside its [`counting_spawns`]
/// scope; 0 outside one.
pub fn scoped_git_spawns() -> u64 {
    SCOPED
        .try_with(|n| n.load(Ordering::Relaxed))
        .unwrap_or_default()
}

/// Record one started git process.
pub(crate) fn spawned(subcommand: &'static str, kind: &'static str) {
    SPAWNS.fetch_add(1, Ordering::Relaxed);
    // Outside a `counting_spawns` scope there is no count to add to.
    let _counted = SCOPED
        .try_with(|n| n.fetch_add(1, Ordering::Relaxed))
        .is_ok();
    tracing::debug!(
        target: "forge_git::spawn",
        subcommand,
        kind,
        "forge.git.spawn"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_scope_counts_the_spawns_of_its_own_task_only() {
        assert_eq!(scoped_git_spawns(), 0, "outside a scope");
        let (mine, other) = counting_spawns(async {
            spawned("rev-parse", "captured");
            spawned("merge-tree", "captured");
            // Another piece of work at the same time has its own count.
            let other = tokio::spawn(counting_spawns(async {
                spawned("cat-file", "batch");
                scoped_git_spawns()
            }))
            .await
            .expect("join");
            (scoped_git_spawns(), other)
        })
        .await;
        assert_eq!((mine, other), (2, 1));
        assert_eq!(scoped_git_spawns(), 0, "the scope ended");
    }
}
