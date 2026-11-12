//! Reachability of a commit named by its id (M3 P6, ADR-0024 §2 and its
//! closing table #25 / #37; see [`super`]).
//!
//! A branch or tag name is reachable by definition; a full commit id is
//! not: a deleted branch's commit, one force-pushed away (a removed
//! secret) or one only `commit-tree` / a mergeability job made stays in the
//! object store until a prune. So an id resolves only when a ref of the
//! [`ReachRoots`] reaches it. Everything an id lookup can find that is not
//! a reachable commit — a missing object, a tree or blob, an unreachable
//! commit, a check that could not decide (its time limit or a full slot
//! table) — is answered exactly like a missing id (`FG-REPO-0023
//! reason=ref_not_found`); only the log tells them apart (`object`,
//! `unreachable`, `reach_error`), so the status never reveals existence.
//!
//! - **Per caller first** (ADR-0024 #37): every id lookup — before the id
//!   is looked at, so a missing id and an existing one count alike — takes
//!   a token of its caller's bucket ([`BrowseLimits::reach_caller_burst`],
//!   one back per [`BrowseLimits::reach_caller_refill`]) and one of the
//!   caller's [`BrowseLimits::reach_per_caller`] slots (waited for at most
//!   [`BrowseLimits::reach_caller_wait`]). An empty bucket is
//!   `FG-GIT-0004 reason=caller_reach_rate`, no slot in time
//!   `reason=caller_reach`: never success. One reader can therefore keep
//!   at most `reach_per_caller` of the repository's `max_per_repo` browse
//!   slots busy with walks. Names never count.
//! - **The check** is one `git rev-list --max-count=1 --stdin --not
//!   --branches --tags [--glob=<extra>…]` with the id on stdin (never on
//!   argv; read where `--stdin` stands, so it is the positive side — the
//!   same shape as the upload-pack want check, `want_check.rs`). Empty
//!   output: a branch or tag reaches the id; any output (the id itself)
//!   means none does. One process and one date-ordered walk for all tips
//!   together. Chosen over the alternatives (measured, git 2.53, 20 000
//!   commits, 6 000 refs): `for-each-ref --count=1 --contains=<id>` costs
//!   the same for a reachable id but ~4–5x more for an unreachable one
//!   (≈1.1 s vs ≈0.25 s) — the case a prober controls — because it decides
//!   every ref (`--count` only cuts the output); `merge-base --is-ancestor
//!   <id> <tip>` is one process per tip. All options used exist in git ≥
//!   2.38.
//! - **Commit-graph**: forge's repositories run with `gc.auto=0` /
//!   `receive.autogc=false` (`storage.rs`), so none is written today and the
//!   walk reads commits one by one; `core.commitGraph` is on by default, so
//!   the scheduled maintenance (plan 11 O3) that writes one speeds this up
//!   without a change here. Correctness never depends on it.
//! - **Costs**: the check has its own time limit,
//!   [`BrowseLimits::reach_timeout`] (and never outlives the operation's
//!   deadline); running out is `FG-GIT-0002 check=reachability`. It takes
//!   one slot of the repository's browse cap (`FG-GIT-0004
//!   reason=repo_browse`) and one of the global git pool. Neither is ever
//!   "reachable"; on an id lookup they answer `ref_not_found` (the real
//!   code is logged once here, `forge.git.browse.reach_undecided`, and kept
//!   as `reach_error`).
//! - **Known residual — timing**: a missing id is answered after one
//!   `cat-file`, an existing unreachable commit after the walk as well, so
//!   response time can still tell them apart (ADR-0024 §2). The per-caller
//!   limits bound how fast anyone can probe; hiding it fully would need
//!   padding every answer to the walk's worst case.
//! - **Shortcuts and cache**: an id that is the commit of a branch or tag in
//!   the cached ref list needs no git at all. Answers are kept per
//!   `(repo, id, roots)` with that repository's cache generation (bumped by
//!   every push through [`Browser::invalidate`]) for at most
//!   [`BrowseLimits::refs_ttl`]: a push to the repository — a force-push
//!   removing a secret included — ends its cached answers at once (and
//!   leaves other repositories' alone); a ref changed outside forge's push
//!   path is seen within the TTL, as with the ref list. A full cache drops
//!   its expired answers, then its oldest one.
//!
//! [`BrowseLimits::reach_caller_burst`]: super::BrowseLimits::reach_caller_burst
//! [`BrowseLimits::reach_caller_refill`]: super::BrowseLimits::reach_caller_refill
//! [`BrowseLimits::reach_per_caller`]: super::BrowseLimits::reach_per_caller
//! [`BrowseLimits::reach_caller_wait`]: super::BrowseLimits::reach_caller_wait
//! [`BrowseLimits::reach_timeout`]: super::BrowseLimits::reach_timeout
//! [`BrowseLimits::refs_ttl`]: super::BrowseLimits::refs_ttl

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use super::{not_resolved, BrowseLimits, Browser, Deadline};

/// Bytes of the reachability answer read at most (one object id).
const REACH_OUTPUT_BYTES: usize = 4 * 1024;

/// Callers remembered at once (beyond it idle callers go first, then the
/// one seen longest ago that holds no slot).
const MAX_CALLERS: usize = 10_000;

/// The refs a commit named by id must be reachable from: always every
/// branch and tag; a merge request view adds that merge request's version
/// refs (`refs/merge-requests/<iid>/*`, M3 R2: [`ReachRoots::merge_request`]).
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

    /// Branches, tags and the hidden refs of merge request `iid` of the
    /// repository (`refs/merge-requests/<iid>/*`: its `v<n>` and `head`;
    /// M3 R2, ADR-0024 §2). Only that merge request's refs: a commit only
    /// another merge request's version reaches stays unreachable here.
    pub fn merge_request(iid: i64) -> Result<Self, ForgeError> {
        let prefix = crate::hidden_refs::merge_request_ref_prefix(iid)?;
        Ok(Self {
            extra: vec![format!("{prefix}*")],
        })
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

/// One caller's share: its slots and its token bucket.
#[derive(Debug)]
struct CallerReach {
    slots: Arc<Semaphore>,
    tokens: u32,
    /// When `tokens` was last brought up to date.
    at: Instant,
}

impl CallerReach {
    /// Give back the tokens earned since `at` (at most `burst`).
    fn refill(&mut self, now: Instant, burst: u32, every: Duration) {
        if self.tokens >= burst {
            self.at = now;
            return;
        }
        let earned = now.saturating_duration_since(self.at).as_nanos() / every.as_nanos().max(1);
        if earned == 0 {
            return;
        }
        let earned = u32::try_from(earned).unwrap_or(u32::MAX);
        self.tokens = self.tokens.saturating_add(earned).min(burst);
        self.at = if self.tokens >= burst {
            now
        } else {
            // Keep the part of an interval already waited for.
            self.at
                .checked_add(every.saturating_mul(earned))
                .unwrap_or(now)
        };
    }

    /// Nobody holds or waits for a slot.
    fn idle(&self) -> bool {
        Arc::strong_count(&self.slots) == 1
    }
}

/// Recent reachability answers and the per-caller limits (see the module
/// documentation).
#[derive(Debug, Default)]
pub(super) struct ReachCache {
    entries: Mutex<HashMap<ReachKey, ReachHit>>,
    callers: Mutex<HashMap<String, CallerReach>>,
}

impl ReachCache {
    /// Drop `repo`'s answers (its refs changed; its new generation already
    /// made them stale — this frees them).
    pub(super) fn forget(&self, repo: RepoId) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|(r, _, _), _| *r != repo);
    }

    /// The ids `repo` has answers for (tests).
    #[cfg(test)]
    pub(super) fn cached_ids(&self, repo: RepoId) -> Vec<String> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .keys()
            .filter(|(r, _, _)| *r == repo)
            .map(|(_, id, _)| id.clone()) // clone: the test owns its list
            .collect()
    }

    /// Take one token of `caller`'s bucket and hand back its slots, or
    /// `FG-GIT-0004 reason=caller_reach_rate` when the bucket is empty.
    fn take_token(
        &self,
        caller: &str,
        limits: &BrowseLimits,
    ) -> Result<Arc<Semaphore>, ForgeError> {
        let burst = limits.reach_caller_burst.max(1);
        let every = limits.reach_caller_refill;
        let now = Instant::now();
        let mut callers = self.callers.lock().unwrap_or_else(PoisonError::into_inner);
        if callers.len() >= MAX_CALLERS && !callers.contains_key(caller) {
            // An idle caller whose bucket is full again has nothing to keep.
            let full_after = every.saturating_mul(burst);
            callers.retain(|_, c| !c.idle() || now.saturating_duration_since(c.at) < full_after);
            if callers.len() >= MAX_CALLERS {
                let oldest = callers
                    .iter()
                    .filter(|(_, c)| c.idle())
                    .min_by_key(|(_, c)| c.at)
                    .map(|(k, _)| k.clone()); // clone: the key outlives the borrow
                if let Some(oldest) = oldest {
                    callers.remove(&oldest);
                }
            }
        }
        let entry = callers
            .entry(caller.to_owned())
            .or_insert_with(|| CallerReach {
                slots: Arc::new(Semaphore::new(limits.reach_per_caller.max(1))),
                tokens: burst,
                at: now,
            });
        entry.refill(now, burst, every);
        if entry.tokens == 0 {
            return Err(ForgeError::new(ErrorCode::GitBusy)
                .with_context("reason", "caller_reach_rate")
                .with_context("burst", burst)
                .with_context("refill_ms", every.as_millis()));
        }
        entry.tokens -= 1;
        Ok(Arc::clone(&entry.slots))
    }
}

/// A check that could not decide (`FG-GIT-0002` / `FG-GIT-0004`): logged
/// here with its own code, answered like a missing id.
fn undecided(err: &ForgeError, repo: RepoId) -> ForgeError {
    tracing::warn!(
        target: "forge_git::browse",
        error_code = err.code().id(),
        error = %err,
        repo_id = repo.get(),
        "forge.git.browse.reach_undecided"
    );
    not_resolved("ref_not_found").with_context("reach_error", err.code().id())
}

impl Browser {
    /// The commit a full id names, reachable from a branch or a tag;
    /// counted against `caller` (see the module documentation).
    pub(super) async fn commit_of(
        &self,
        repo: RepoId,
        id: &str,
        caller: &str,
        deadline: Deadline,
    ) -> Result<String, ForgeError> {
        self.commit_reachable_from(repo, id, &ReachRoots::branches_and_tags(), caller, deadline)
            .await
    }

    /// The commit a full id names, if a ref of `roots` reaches it;
    /// anything else is `ref_not_found`, exactly like a missing id.
    pub(super) async fn commit_reachable_from(
        &self,
        repo: RepoId,
        id: &str,
        roots: &ReachRoots,
        caller: &str,
        deadline: Deadline,
    ) -> Result<String, ForgeError> {
        let _permit = self.caller_gate(caller, deadline).await?;
        let commit = self.commit_exists(repo, id, deadline).await?;
        match self.reachable(repo, &commit, roots, deadline).await {
            Ok(true) => Ok(commit),
            Ok(false) => Err(not_resolved("ref_not_found").with_context("unreachable", true)),
            Err(err) => {
                let code = err.code();
                if code == ErrorCode::GitTimeout || code == ErrorCode::GitBusy {
                    Err(undecided(&err, repo))
                } else {
                    Err(err)
                }
            }
        }
    }

    /// The commit `id` (a full id, any case) in the view of merge request
    /// `iid` (M3 R2): reachable from a branch, a tag or one of that merge
    /// request's version refs ([`ReachRoots::merge_request`]); counted
    /// against `caller` like every id lookup. A purged version (its refs
    /// gone) or a commit only a trial merge made answers like a missing id
    /// (`FG-REPO-0023 reason=ref_not_found`).
    pub async fn merge_request_commit(
        &self,
        repo: RepoId,
        iid: i64,
        id: &str,
        caller: &str,
    ) -> Result<String, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let id = super::object_id(id).ok_or_else(|| not_resolved("ref_not_found"))?;
        let roots = ReachRoots::merge_request(iid)?;
        self.commit_reachable_from(repo, &id, &roots, caller, deadline)
            .await
    }

    /// `from` → `to` (full ids) compared in the view of merge request
    /// `iid`: both are checked like [`Self::merge_request_commit`] first,
    /// then [`Self::compare`] runs on the resolved ids.
    pub async fn merge_request_compare(
        &self,
        repo: RepoId,
        iid: i64,
        from: &str,
        to: &str,
        caller: &str,
    ) -> Result<super::Comparison, ForgeError> {
        let from = self.merge_request_commit(repo, iid, from, caller).await?;
        let to = self.merge_request_commit(repo, iid, to, caller).await?;
        self.compare(repo, &from, &to).await
    }

    /// One token and one slot of `caller` (held for the whole lookup).
    pub(super) async fn caller_gate(
        &self,
        caller: &str,
        deadline: Deadline,
    ) -> Result<OwnedSemaphorePermit, ForgeError> {
        let slots = self.reach.take_token(caller, &self.limits)?;
        let wait = deadline.left()?.min(self.limits.reach_caller_wait);
        match tokio::time::timeout(wait, slots.acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            // A closed semaphore is a bug, not load.
            Ok(Err(e)) => Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "caller_slots_closed")
                .with_source(e)),
            Err(_) => Err(ForgeError::new(ErrorCode::GitBusy)
                .with_context("reason", "caller_reach")
                .with_context("max", self.limits.reach_per_caller)
                .with_context("wait_ms", wait.as_millis())),
        }
    }

    /// The commit a full id names, reachable or not; a missing object or
    /// one of another type is `ref_not_found` (the log says which).
    async fn commit_exists(
        &self,
        repo: RepoId,
        id: &str,
        deadline: Deadline,
    ) -> Result<String, ForgeError> {
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        match self.fast_info(repo, &dir, id, deadline).await {
            Some(crate::batch::Info::Found { kind, .. }) if kind == "commit" => {
                return Ok(id.to_owned())
            }
            Some(crate::batch::Info::Found { .. }) => {
                return Err(not_resolved("ref_not_found").with_context("object", "not_a_commit"))
            }
            Some(crate::batch::Info::Missing) => return Err(not_resolved("ref_not_found")),
            None => {}
        }
        let out = self
            .git
            .command("cat-file")
            .arg("--batch-check=%(objectname) %(objecttype)")
            .repo(&dir)
            .stdin(format!("{id}\n").into_bytes())
            .timeout(deadline.left()?)
            .output()
            .await?;
        let text = out.stdout_str();
        let mut fields = text.split_whitespace();
        match (fields.next(), fields.next()) {
            (Some(found), Some("commit")) if found == id => Ok(id.to_owned()),
            (Some(_), Some("missing")) => Err(not_resolved("ref_not_found")),
            (None, _) => Err(ForgeError::new(ErrorCode::GitProcessFailed)
                .with_context("reason", "batch_check_empty")
                .with_context("repo_id", repo.get())),
            _ => Err(not_resolved("ref_not_found").with_context("object", "not_a_commit")),
        }
    }

    /// Whether a ref of `roots` reaches the commit `id` (a full, lower-case
    /// commit id the caller has seen to be a commit). Errors are the
    /// check's own (`check=reachability`), never "reachable".
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
        let epoch = self.epoch(repo);
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
        self.reach_store(key, epoch, reachable);
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
            .stdin(format!("{id}\n").into_bytes())
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

    /// Keep an answer computed at generation `epoch` — unless the
    /// repository was pushed meanwhile (checked under the lock, so an
    /// invalidation either sees the entry and drops it or makes this skip).
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
        if self.epoch(key.0) != epoch {
            return;
        }
        if entries.len() >= max && !entries.contains_key(&key) {
            entries.retain(|_, h| h.at.elapsed() < ttl);
            while entries.len() >= max {
                let oldest = entries
                    .iter()
                    .min_by_key(|(_, h)| h.at)
                    .map(|(k, _)| k.clone()); // clone: the key outlives the borrow
                match oldest {
                    Some(oldest) => {
                        entries.remove(&oldest);
                    }
                    None => break,
                }
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
