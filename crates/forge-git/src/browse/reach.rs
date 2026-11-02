//! Reachability of a commit named by its id (M3 P6, ADR-0024 §2 and its
//! closing table #25 / #37; see [`super`]).
//!
//! A branch or tag name is reachable by definition; a full commit id is
//! not: a deleted branch's commit, one force-pushed away (a removed
//! secret) or one only `commit-tree` / a mergeability job made stays in the
//! object store until a prune. So an id resolves only when a ref of the
//! [`ReachRoots`] reaches it; otherwise the caller gets exactly what an id
//! the repository does not have gets (`FG-REPO-0023 reason=ref_not_found`)
//! — existence never leaks.
//!
//! - **The check** is one `git rev-list --max-count=1 --stdin --not
//!   --branches --tags [--glob=<extra>…]` with the id on stdin (never on
//!   argv; read where `--stdin` stands, so it is the positive side — the
//!   same shape as the upload-pack want check, `want_check.rs`). Empty
//!   output: every ref-excluded walk ended inside the refs' history, so a
//!   branch or tag reaches the id; any output (the id itself) means none
//!   does. One process and one date-ordered walk for all tips together.
//!   Chosen over the alternatives (measured, git 2.53, 20 000 commits,
//!   6 000 refs): `for-each-ref --count=1 --contains=<id>` costs the same
//!   for a reachable id but ~4–5x more for an unreachable one (≈1.1 s vs
//!   ≈0.25 s) — the case a prober controls — because it decides every ref
//!   (`--count` only cuts the output);
//!   `merge-base --is-ancestor <id> <tip>` is one process per tip. All
//!   options used exist in git ≥ 2.38.
//! - **Commit-graph**: forge's repositories run with `gc.auto=0` /
//!   `receive.autogc=false` (`storage.rs`), so none is written today and the
//!   walk reads commits one by one; `core.commitGraph` is on by default, so
//!   the scheduled maintenance (plan 11 O3) that writes one speeds this up
//!   without a change here. Correctness never depends on it.
//! - **Costs**: the check has its own time limit,
//!   [`super::BrowseLimits::reach_timeout`] (and never outlives the operation's
//!   deadline); running out is `FG-GIT-0002` with `check=reachability`. It
//!   takes one slot of the repository's browse cap (`FG-GIT-0004
//!   reason=repo_browse`) and one of the global git pool (`FG-GIT-0004`) —
//!   the same per-repository limit every browse call counts against
//!   (M2 has no per-caller limit on the JSON browse API to reuse; the
//!   streams' per-principal cap does not apply to captured calls). An error
//!   is never "reachable": every failure propagates.
//! - **Shortcuts and cache**: an id that is the commit of a branch or tag in
//!   the cached ref list needs no git at all. Answers are kept per
//!   `(repo, id, roots)` together with the ref cache's generation (the
//!   epoch every push bumps through [`Browser::invalidate`]) for at most
//!   [`super::BrowseLimits::refs_ttl`]: a push — a force-push removing a secret
//!   included — ends every cached answer at once; a ref changed outside
//!   forge's push path is seen within the TTL, as with the ref list.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, PoisonError};

use forge_core::RepoId;
use forge_errors::ForgeError;
use tokio::time::Instant;

use super::{Browser, Deadline};

/// Bytes of the reachability answer read at most (one object id).
const REACH_OUTPUT_BYTES: usize = 4 * 1024;

/// The refs a commit named by id must be reachable from: always every
/// branch and tag; a merge request view adds that merge request's version
/// refs (`refs/merge-requests/<iid>/`, M3 R1 — not yet).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReachRoots {
    /// More `rev-list --glob` patterns (forge-built `refs/…/*` only;
    /// never user input).
    extra: Vec<String>,
}

impl ReachRoots {
    /// Branches and tags only (every browse route outside a merge request).
    pub fn branches_and_tags() -> Self {
        Self::default()
    }

    /// The cache key part (patterns cannot hold a newline).
    fn key(&self) -> String {
        self.extra.join("\n")
    }
}

type ReachKey = (RepoId, String, String);

#[derive(Debug, Clone, Copy)]
struct ReachHit {
    at: Instant,
    epoch: u64,
    reachable: bool,
}

/// Recent reachability answers (see the module documentation).
#[derive(Debug, Default)]
pub(super) struct ReachCache {
    entries: Mutex<HashMap<ReachKey, ReachHit>>,
}

impl ReachCache {
    /// Drop `repo`'s answers (its refs changed; the epoch already made
    /// them stale — this frees them).
    pub(super) fn forget(&self, repo: RepoId) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|(r, _, _), _| *r != repo);
    }
}

impl Browser {
    /// Whether a ref of `roots` reaches the commit `id` (a full, lower-case
    /// commit id the caller has seen to be a commit).
    pub(super) async fn reachable(
        &self,
        repo: RepoId,
        id: &str,
        roots: &ReachRoots,
        deadline: Deadline,
    ) -> Result<bool, ForgeError> {
        if let Some(list) = self.cached(repo) {
            if list.refs.iter().any(|r| r.commit.as_deref() == Some(id)) {
                return Ok(true);
            }
        }
        let key: ReachKey = (repo, id.to_owned(), roots.key());
        let epoch = self.cache.epoch.load(Ordering::SeqCst);
        if let Some(reachable) = self.reach_hit(&key, epoch) {
            return Ok(reachable);
        }
        let reachable = self
            .ask_reachable(repo, id, roots, deadline)
            .await
            .map_err(|e| {
                e.with_context("check", "reachability")
                    .with_context("repo_id", repo.get())
            })?;
        // An answer that may predate a push is not kept.
        if self.cache.epoch.load(Ordering::SeqCst) == epoch {
            self.reach_store(key, epoch, reachable);
        }
        Ok(reachable)
    }

    async fn ask_reachable(
        &self,
        repo: RepoId,
        id: &str,
        roots: &ReachRoots,
        deadline: Deadline,
    ) -> Result<bool, ForgeError> {
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let budget = deadline.left()?.min(self.limits.reach_timeout);
        let mut cmd = self
            .git
            .command("rev-list")
            .arg("--max-count=1")
            // Read where it stands: before `--not`, the id is the positive
            // side (pinned for the want check by its own test).
            .arg("--stdin")
            .arg("--not")
            .arg("--branches")
            .arg("--tags");
        for pattern in &roots.extra {
            cmd = cmd.ref_glob(pattern);
        }
        let (out, _cut) = cmd
            .repo(&dir)
            .stdin(
                format!(
                    "{id}
"
                )
                .into_bytes(),
            )
            .timeout(budget)
            .output_truncated(REACH_OUTPUT_BYTES)
            .await?;
        // Nothing printed: no commit the id reaches is outside the refs.
        Ok(out.stdout.iter().all(u8::is_ascii_whitespace))
    }

    fn reach_hit(&self, key: &ReachKey, epoch: u64) -> Option<bool> {
        let entries = self
            .reach
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        entries
            .get(key)
            .filter(|h| h.epoch == epoch && h.at.elapsed() < self.limits.refs_ttl)
            .map(|h| h.reachable)
    }

    fn reach_store(&self, key: ReachKey, epoch: u64, reachable: bool) {
        let max = self.limits.max_cached_reach;
        if max == 0 {
            return;
        }
        let ttl = self.limits.refs_ttl;
        let mut entries = self
            .reach
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= max && !entries.contains_key(&key) {
            entries.retain(|_, h| h.epoch == epoch && h.at.elapsed() < ttl);
            if entries.len() >= max {
                // Every answer is fresh: start over rather than scan for the
                // oldest on every store (answers are cheap to recompute).
                entries.clear();
            }
        }
        entries.insert(
            key,
            ReachHit {
                at: Instant::now(),
                epoch,
                reachable,
            },
        );
    }
}
