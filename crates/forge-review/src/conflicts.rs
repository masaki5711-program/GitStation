//! Predicting conflicts between open merge requests (M3 X6, plan 04 C-2,
//! ADR-0024 Amendment X6): the body of the job `merge_request.conflict_scan`
//! (`forge worker` runs it), its end-of-life path, and the version job's
//! request after a new head.
//!
//! One scan is about one merge request M (its recorded head H, its target
//! branch T) and every other open merge request on T — **drafts included** —
//! the [`ScanLimits::peers_max`] most recently updated:
//!
//! 1. [`forge_db::review::begin_scan`]: a run whose `scan_seq` is no longer
//!    the latest stops (`superseded`); otherwise the row is `scanning`.
//! 2. Narrow first (cheap): T's tip, then for M and each peer P the paths
//!    changed since its merge base with the tip (`merge-base`,
//!    `diff-tree --no-renames`). A pair whose changed paths do not
//!    intersect is `no_overlap` — no trial merge runs. A side with more than
//!    [`ScanLimits::paths_max`] paths (or a non-UTF-8 one) has an unknown
//!    set: the pair stays a candidate (never "no conflict" by default).
//! 3. Then `merge-tree --write-tree` of H and P's head, for at most
//!    [`ScanLimits::pairs_max`] candidates: a conflict in a path both change
//!    (any conflict when a set is unknown, or a list cut by its cap) is a
//!    predicted `conflict`; otherwise `clean` — a conflict only in paths
//!    one side does not change is that side's conflict with the target,
//!    which the mergeability check (C-1) reports.
//! 4. [`forge_db::review::record_scan`] stores the scan and its pairs while
//!    the seq is the latest and M's head and target are the ones scanned; a
//!    pair that ENTERS `conflict` writes `merge_request.conflict_predicted`
//!    for both sides.
//!
//! Bounds: every git call has its own timeout and the scan as a whole a
//! budget ([`ScanLimits::budget`]); a candidate left out by a cap or the
//! budget is counted (`pairs_skipped`) and the scan is `incomplete` with the
//! reason, logged once at warn with `FG-REVIEW-0022`. Undecidable cases are
//! stored, never "no conflict": no version → `failed no_version`; the target
//! gone → `target_missing`; M sharing no history with it →
//! `unrelated_histories`; a peer sharing none → that pair `failed
//! unrelated_histories`. A git failure on M is stored as `failed git_*`
//! and returned (the worker retries); on one pair it is stored on the pair,
//! the scan is `incomplete pair_failed`, and the first error is returned
//! after the store (logged by the worker with its own code; retried).

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use forge_core::ids::MergeRequestId;
use forge_core::{ObjectId, RefName, RepoId, RequestId};
use forge_db::review::{
    self, ConflictScanRequest, PairOutcome, PairResult, Peer, ScanRecorded, ScanResult, ScanStart,
    ScanStatus,
};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::mergeability::{MergeTree, MergeTreeLimits};
use forge_git::review_inputs::ChangedPaths;
use forge_git::{Git, StorageRoot};

use crate::mergeability::{git_reason, stored_files, system};

/// The bounds of one scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    /// Other open merge requests considered (the most recently updated).
    pub peers_max: usize,
    /// Trial merges per scan.
    pub pairs_max: usize,
    /// Changed paths read per side (more: the set is unknown).
    pub paths_max: usize,
    /// The whole scan's git time.
    pub budget: Duration,
    /// One `merge-base`, `diff-tree` or ref read.
    pub read_timeout: Duration,
    /// One trial merge (its timeout is cut to the budget left).
    pub merge_tree: MergeTreeLimits,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            peers_max: 50,
            pairs_max: 20,
            paths_max: 1_000,
            budget: Duration::from_secs(120),
            read_timeout: Duration::from_secs(10),
            merge_tree: MergeTreeLimits::default(),
        }
    }
}

/// Where the scan runs: git, the storage, the bounds.
#[derive(Clone)]
pub struct ConflictPredictor {
    git: Git,
    storage: StorageRoot,
    limits: ScanLimits,
}

impl std::fmt::Debug for ConflictPredictor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConflictPredictor")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// What a run did (the job's log line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// `scanned`, `superseded`, `not_open` or `gone`.
    pub outcome: &'static str,
    pub status: Option<ScanStatus>,
    pub reason: Option<&'static str>,
    pub pairs_tried: i32,
    pub pairs_skipped: i32,
    /// The other merge requests now predicted to conflict (new only).
    pub predicted: Vec<MergeRequestId>,
}

impl ScanReport {
    fn skipped(outcome: &'static str) -> Self {
        Self {
            outcome,
            status: None,
            reason: None,
            pairs_tried: 0,
            pairs_skipped: 0,
            predicted: Vec::new(),
        }
    }
}

/// The changed paths of one side: `None` when unknown (too many, not
/// UTF-8).
type Paths = Option<BTreeSet<String>>;

/// One candidate pair (it changes a path the scanned merge request
/// changes, or a set is unknown).
struct Candidate<'a> {
    peer: &'a Peer,
    /// `None`: a set is unknown, every conflicted path counts.
    overlap: Option<BTreeSet<String>>,
}

fn i32_of(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn pair_of(peer: &Peer, outcome: PairOutcome, reason: Option<&'static str>) -> PairResult {
    PairResult {
        other: peer.merge_request,
        other_iid: peer.iid,
        other_head: peer.head.clone(), // clone: the result owns its ids
        outcome,
        reason,
        conflicts: Vec::new(),
        conflicts_truncated: false,
    }
}

/// What the git part found, and the first git failure (stored first).
struct Computed {
    result: ScanResult,
    git_error: Option<ForgeError>,
}

impl ConflictPredictor {
    pub fn new(git: Git, storage: StorageRoot) -> Self {
        Self {
            git,
            storage,
            limits: ScanLimits::default(),
        }
    }

    /// Other bounds (tests).
    #[must_use]
    pub fn with_limits(mut self, limits: ScanLimits) -> Self {
        self.limits = limits;
        self
    }

    /// A read's timeout: its own, cut to what is left of the budget.
    fn timeout(&self, deadline: Instant, own: Duration) -> Duration {
        own.min(deadline.saturating_duration_since(Instant::now()))
    }

    /// The changed paths of `head` since its merge base with `tip`
    /// (`Ok(None)` for the outer option: no common history).
    async fn changed(
        &self,
        repo: RepoId,
        tip: &ObjectId,
        head: &ObjectId,
        deadline: Instant,
    ) -> Result<Option<Paths>, ForgeError> {
        let read = self.timeout(deadline, self.limits.read_timeout);
        let Some(base) = self
            .git
            .merge_base(&self.storage, repo, tip, head, read)
            .await?
        else {
            return Ok(None);
        };
        let read = self.timeout(deadline, self.limits.read_timeout);
        let paths = self
            .git
            .changed_paths(
                &self.storage,
                repo,
                &base,
                head,
                self.limits.paths_max,
                read,
            )
            .await?;
        Ok(Some(match paths {
            ChangedPaths::Complete(paths) => Some(paths.into_iter().collect()),
            ChangedPaths::TooMany | ChangedPaths::NonUtf8 => None,
        }))
    }

    /// The git part (module docs, steps 2 and 3).
    async fn compute(
        &self,
        repo: RepoId,
        head: Option<&ObjectId>,
        target: &RefName,
        peers: &[Peer],
        total_peers: i64,
    ) -> Computed {
        let deadline = Instant::now() + self.limits.budget;
        let failed = |reason: &'static str, head: Option<&ObjectId>| ScanResult {
            status: ScanStatus::Failed,
            reason: Some(reason),
            head: head.cloned(),           // clone: the result owns its ids
            target_branch: target.clone(), // clone: as above
            peers: 0,
            pairs_tried: 0,
            pairs_skipped: 0,
            pairs: Vec::new(),
        };
        let done = |result| Computed {
            result,
            git_error: None,
        };
        let Some(head) = head else {
            return done(failed("no_version", None));
        };
        let read = self.timeout(deadline, self.limits.read_timeout);
        let tip = match self.git.read_ref(&self.storage, repo, target, read).await {
            Ok(Some(tip)) => tip,
            Ok(None) => return done(failed("target_missing", Some(head))),
            Err(e) => {
                return Computed {
                    result: failed(git_reason(&e).as_str(), Some(head)),
                    git_error: Some(e),
                }
            }
        };
        let own = match self.changed(repo, &tip, head, deadline).await {
            Ok(Some(paths)) => paths,
            Ok(None) => return done(failed("unrelated_histories", Some(head))),
            Err(e) => {
                return Computed {
                    result: failed(git_reason(&e).as_str(), Some(head)),
                    git_error: Some(e),
                }
            }
        };
        let mut pairs = Vec::new();
        let mut candidates = Vec::new();
        let mut git_error: Option<ForgeError> = None;
        let mut budget_out = false;
        let mut skipped = 0usize;
        for peer in peers {
            if Instant::now() >= deadline {
                budget_out = true;
                skipped += 1;
                continue;
            }
            match self.changed(repo, &tip, &peer.head, deadline).await {
                Ok(None) => pairs.push(pair_of(
                    peer,
                    PairOutcome::Failed,
                    Some("unrelated_histories"),
                )),
                Ok(Some(theirs)) => {
                    let overlap = match (&own, &theirs) {
                        (Some(mine), Some(theirs)) => {
                            let common: BTreeSet<String> =
                                mine.intersection(theirs).cloned().collect(); // clone: the set owns its paths
                            if common.is_empty() {
                                pairs.push(pair_of(peer, PairOutcome::NoOverlap, None));
                                continue;
                            }
                            Some(common)
                        }
                        _ => None,
                    };
                    candidates.push(Candidate { peer, overlap });
                }
                Err(e) => {
                    pairs.push(pair_of(
                        peer,
                        PairOutcome::Failed,
                        Some(git_reason(&e).as_str()),
                    ));
                    git_error.get_or_insert(e);
                }
            }
        }
        let capped = candidates.len() > self.limits.pairs_max;
        let mut tried = 0usize;
        for (n, candidate) in candidates.into_iter().enumerate() {
            if n >= self.limits.pairs_max {
                skipped += 1;
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                budget_out = true;
                skipped += 1;
                continue;
            }
            let limits = MergeTreeLimits {
                timeout: self.limits.merge_tree.timeout.min(left),
                ..self.limits.merge_tree
            };
            tried += 1;
            let peer = candidate.peer;
            match self
                .git
                .merge_tree(&self.storage, repo, head, &peer.head, limits)
                .await
            {
                Ok(MergeTree::Clean { .. }) => pairs.push(pair_of(peer, PairOutcome::Clean, None)),
                Ok(MergeTree::Conflicted {
                    files,
                    files_truncated,
                }) => {
                    let relevant: Vec<String> = match &candidate.overlap {
                        Some(common) => files.into_iter().filter(|f| common.contains(f)).collect(),
                        None => files,
                    };
                    if relevant.is_empty() && !files_truncated {
                        // Conflicts only where one side alone changes: that
                        // side's conflict with the target (C-1's), not theirs.
                        pairs.push(pair_of(peer, PairOutcome::Clean, None));
                    } else {
                        let (conflicts, cut) = stored_files(&relevant);
                        pairs.push(PairResult {
                            conflicts,
                            conflicts_truncated: files_truncated || cut,
                            ..pair_of(peer, PairOutcome::Conflict, None)
                        });
                    }
                }
                Err(e) => {
                    pairs.push(pair_of(
                        peer,
                        PairOutcome::Failed,
                        Some(git_reason(&e).as_str()),
                    ));
                    git_error.get_or_insert(e);
                }
            }
        }
        let peers_capped = i64::try_from(peers.len()).unwrap_or(i64::MAX) < total_peers;
        let reason = if git_error.is_some() {
            Some("pair_failed")
        } else if budget_out {
            Some("budget_exhausted")
        } else if capped {
            Some("pairs_capped")
        } else if peers_capped {
            Some("peers_capped")
        } else {
            None
        };
        Computed {
            result: ScanResult {
                status: if reason.is_some() {
                    ScanStatus::Incomplete
                } else {
                    ScanStatus::Scanned
                },
                reason,
                head: Some(head.clone()), // clone: the result owns its ids
                target_branch: target.clone(), // clone: as above
                peers: i32_of(peers.len()),
                pairs_tried: i32_of(tried),
                pairs_skipped: i32_of(skipped),
                pairs,
            },
            git_error,
        }
    }
}

/// The scan job (module docs). Errors: forge-db's (`FG-DB-0001` / `0002`),
/// `FG-REPO-0016` for a repository not ready, and a git failure after it
/// was stored; each is the job's failure (the worker logs it once with the
/// job's request id and retries).
pub async fn scan(
    db: &Db,
    predictor: &ConflictPredictor,
    request: ConflictScanRequest,
    request_id: &RequestId,
) -> Result<ScanReport, ForgeError> {
    let started = Instant::now();
    let ConflictScanRequest {
        merge_request: mr,
        scan_seq: seq,
    } = request;
    if review::begin_scan(db, mr, seq).await? == ScanStart::Superseded {
        return Ok(ScanReport::skipped("superseded"));
    }
    let Some(target) = review::refresh_target(db, mr).await? else {
        return Ok(ScanReport::skipped("gone"));
    };
    let stored = &target.merge_request;
    if stored.state != forge_core::MergeRequestState::Open {
        // A reopen requests a new scan; the reaper skips closed ones.
        return Ok(ScanReport::skipped("not_open"));
    }
    if !target.repository_ready {
        return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
            .with_context("repo_id", target.repository.get())
            .with_context("merge_request_id", mr.get()));
    }
    let head = stored
        .head_sha
        .as_deref()
        .map(ObjectId::parse)
        .transpose()?;
    let limit = i64::try_from(predictor.limits.peers_max).unwrap_or(i64::MAX);
    let (peers, total) = if head.is_some() {
        review::scan_peers(db, stored.project_id, &stored.target_branch, mr, limit).await?
    } else {
        (Vec::new(), 0)
    };
    let Computed { result, git_error } = predictor
        .compute(
            target.repository,
            head.as_ref(),
            &stored.target_branch,
            &peers,
            total,
        )
        .await;
    let recorded = review::record_scan(db, &system(request_id), mr, seq, &result).await;
    let recorded = match (recorded, git_error) {
        (Ok(_), Some(e)) => {
            log_scan(request_id, mr, seq, stored.iid, &result, &[], started);
            return Err(e
                .with_context("merge_request_id", mr.get())
                .with_context("scan_seq", seq)
                .with_context("stored", result.status.as_str()));
        }
        (Err(db_error), Some(e)) => return Err(db_error.with_context("git_error", e.code().id())),
        (Err(db_error), None) => return Err(db_error),
        (Ok(recorded), None) => recorded,
    };
    let (outcome, predicted) = match recorded {
        ScanRecorded::Superseded => ("superseded", Vec::new()),
        ScanRecorded::Stored { predicted, .. } => ("scanned", predicted),
    };
    if outcome == "scanned" {
        log_scan(
            request_id, mr, seq, stored.iid, &result, &predicted, started,
        );
    }
    Ok(ScanReport {
        outcome,
        status: Some(result.status),
        reason: result.reason,
        pairs_tried: result.pairs_tried,
        pairs_skipped: result.pairs_skipped,
        predicted,
    })
}

/// The scan's one log line: info when every candidate was tried, warn with
/// `FG-REVIEW-0022` when incomplete, warn when nothing could be predicted.
fn log_scan(
    request_id: &RequestId,
    mr: MergeRequestId,
    seq: i64,
    iid: i64,
    result: &ScanResult,
    predicted: &[MergeRequestId],
    started: Instant,
) {
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let conflicts: Vec<i64> = result
        .pairs
        .iter()
        .filter(|p| p.outcome == PairOutcome::Conflict)
        .map(|p| p.other_iid)
        .collect();
    let predicted: Vec<i64> = predicted.iter().map(|m| m.get()).collect();
    let head_sha = result.head.as_ref().map(ObjectId::as_str);
    match result.status {
        ScanStatus::Incomplete => {
            let reason = result.reason.unwrap_or("unknown");
            let e = review::prediction_incomplete(reason);
            tracing::warn!(
                target: "forge_review",
                event = "forge.review.conflict_scan_incomplete",
                error_code = e.code().id(),
                request_id = %request_id,
                merge_request_id = mr.get(),
                iid,
                scan_seq = seq,
                reason,
                head_sha,
                peers = result.peers,
                pairs_tried = result.pairs_tried,
                pairs_skipped = result.pairs_skipped,
                conflicts_with = ?conflicts,
                predicted = ?predicted,
                duration_ms,
                "the conflict scan did not try every pair"
            );
        }
        ScanStatus::Failed => tracing::warn!(
            target: "forge_review",
            event = "forge.review.conflict_scanned",
            request_id = %request_id,
            merge_request_id = mr.get(),
            iid,
            scan_seq = seq,
            status = result.status.as_str(),
            reason = result.reason,
            head_sha,
            duration_ms,
            "no conflict could be predicted for this merge request"
        ),
        ScanStatus::Scanned | ScanStatus::Pending | ScanStatus::Scanning => tracing::info!(
            target: "forge_review",
            event = "forge.review.conflict_scanned",
            request_id = %request_id,
            merge_request_id = mr.get(),
            iid,
            scan_seq = seq,
            status = result.status.as_str(),
            head_sha,
            peers = result.peers,
            pairs_tried = result.pairs_tried,
            conflicts_with = ?conflicts,
            predicted = ?predicted,
            duration_ms,
            "merge request conflicts predicted"
        ),
    }
}

/// The job failed for good (`cause`: its last error): end its request as
/// `failed reason=job_failed` and log `FG-REVIEW-0022` once (the worker
/// logs `cause` itself). A request superseded or finished is left alone. A
/// failure to write is logged here and not returned: the sweep's reaper
/// ends the row later (`job_stale`).
pub async fn fail(
    db: &Db,
    request: ConflictScanRequest,
    request_id: &RequestId,
    cause: &ForgeError,
) {
    let ConflictScanRequest {
        merge_request: mr,
        scan_seq: seq,
    } = request;
    match review::fail_scan(db, mr, seq).await {
        Ok(true) => {
            let e = review::prediction_incomplete("job_failed");
            tracing::warn!(
                target: "forge_review",
                event = "forge.review.conflict_scan_not_finished",
                error_code = e.code().id(),
                request_id = %request_id,
                reason = "job_failed",
                merge_request_id = mr.get(),
                scan_seq = seq,
                cause_code = cause.code().id(),
                "the conflict scan job failed for good; stored failed"
            );
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(
            target: "forge_review",
            event = "forge.review.conflict_scan_fail_unrecorded",
            error_code = e.code().id(),
            request_id = %request_id,
            merge_request_id = mr.get(),
            scan_seq = seq,
            error = %e,
            "could not store job_failed; the sweep ends the row as job_stale"
        ),
    }
}

/// The version job's request after a version: a scan of `mr` at its
/// current head unless its stored scan already answers it (see
/// `forge_db::review::request_scan_for_head`; nothing before the first
/// version or when the merge request is not open). The scan's seq when one
/// was requested.
pub(crate) async fn after_version(
    db: &Db,
    mr: MergeRequestId,
    request_id: &RequestId,
) -> Result<Option<i64>, ForgeError> {
    let Some(now) = review::refresh_target(db, mr).await? else {
        return Ok(None);
    };
    let Some(head) = now
        .merge_request
        .head_sha
        .as_deref()
        .map(ObjectId::parse)
        .transpose()?
    else {
        return Ok(None);
    };
    review::request_scan_for_head(db, mr, &head, request_id.clone()).await // clone: the job keeps its origin
}

#[cfg(test)]
#[path = "conflicts_tests.rs"]
mod tests;
