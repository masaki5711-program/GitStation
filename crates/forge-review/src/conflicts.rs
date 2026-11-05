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

/// What reading one side found.
enum SideRead {
    /// Its merge base with the target tip and its changed paths.
    Paths { base: ObjectId, paths: Paths },
    /// No common history with the target.
    Unrelated,
    /// The budget ran out before git could run (no process started).
    Budget,
}

/// One candidate pair (it changes a path the scanned merge request
/// changes, or a directory of one, or a set is unknown).
struct Candidate<'a> {
    peer: &'a Peer,
    base: ObjectId,
    /// `None`: a set is unknown.
    theirs: Paths,
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

/// Whether `set` holds a path below directory `dir`.
fn has_below(set: &BTreeSet<String>, dir: &str) -> bool {
    let prefix = format!("{dir}/");
    set.range(prefix.clone()..)
        .next()
        .is_some_and(|p| p.starts_with(&prefix))
}

/// Whether `set` holds `path` or one of its directories.
fn has_at_or_above(set: &BTreeSet<String>, path: &str) -> bool {
    set.contains(path)
        || path
            .match_indices('/')
            .any(|(i, _)| set.contains(&path[..i]))
}

/// Whether the two sides touch a common path: the same path, or a path and
/// a directory of it (a file on one side where the other has a directory
/// of that name — directory/file — shares no exact path; review HIGH).
pub(crate) fn overlaps(mine: &BTreeSet<String>, theirs: &BTreeSet<String>) -> bool {
    mine.iter()
        .any(|p| has_at_or_above(theirs, p) || has_below(theirs, p))
}

/// What a trial merge's conflict means for the pair.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// A conflict between the two (`kept`: the paths to store).
    Conflict { kept: Vec<String> },
    /// Every conflicted path is changed by exactly one side AND by the
    /// target between the two sides' bases: that side's conflict with the
    /// target (C-1's), not theirs.
    TargetOnly,
}

/// The rule (review HIGH): a reported conflict stays a conflict unless
/// every path is changed by exactly one side and by the target between
/// the bases (`target_delta`; `None` when unknown or the bases are the
/// same commit). A rename destination, a rename/rename or a
/// directory/file path is never in the target's delta when the target did
/// not touch it, so it stays a conflict; an empty or cut list, or an
/// unknown side, too.
pub(crate) fn verdict(
    files: Vec<String>,
    truncated: bool,
    mine: Option<&BTreeSet<String>>,
    theirs: Option<&BTreeSet<String>>,
    target_delta: Option<&BTreeSet<String>>,
) -> Verdict {
    let (Some(mine), Some(theirs), Some(delta)) = (mine, theirs, target_delta) else {
        return Verdict::Conflict { kept: files };
    };
    if files.is_empty() || truncated {
        return Verdict::Conflict { kept: files };
    }
    let target_only = |f: &String| mine.contains(f) != theirs.contains(f) && delta.contains(f);
    if files.iter().all(target_only) {
        Verdict::TargetOnly
    } else {
        Verdict::Conflict {
            kept: files.into_iter().filter(|f| !target_only(f)).collect(),
        }
    }
}

/// What the git part found, and a failure on the scanned merge request
/// itself (stored first, then the job's error).
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

    /// A call's timeout: its own, cut to what is left of the budget;
    /// `None` when nothing is left (no git process is started then).
    fn timeout(deadline: Instant, own: Duration) -> Option<Duration> {
        let left = deadline.saturating_duration_since(Instant::now());
        (!left.is_zero()).then(|| own.min(left))
    }

    /// The merge base of `head` with `tip` and the paths changed since.
    async fn changed(
        &self,
        repo: RepoId,
        tip: &ObjectId,
        head: &ObjectId,
        deadline: Instant,
    ) -> Result<SideRead, ForgeError> {
        let Some(read) = Self::timeout(deadline, self.limits.read_timeout) else {
            return Ok(SideRead::Budget);
        };
        let Some(base) = self
            .git
            .merge_base(&self.storage, repo, tip, head, read)
            .await?
        else {
            return Ok(SideRead::Unrelated);
        };
        let Some(read) = Self::timeout(deadline, self.limits.read_timeout) else {
            return Ok(SideRead::Budget);
        };
        let paths = self.paths(repo, &base, head, read).await?;
        Ok(SideRead::Paths { base, paths })
    }

    async fn paths(
        &self,
        repo: RepoId,
        from: &ObjectId,
        to: &ObjectId,
        timeout: Duration,
    ) -> Result<Paths, ForgeError> {
        let paths = self
            .git
            .changed_paths(
                &self.storage,
                repo,
                from,
                to,
                self.limits.paths_max,
                timeout,
            )
            .await?;
        Ok(match paths {
            ChangedPaths::Complete(paths) => Some(paths.into_iter().collect()),
            ChangedPaths::TooMany | ChangedPaths::NonUtf8 => None,
        })
    }

    /// The paths the target changed between the two bases (`None` when the
    /// bases are the same commit, unknown, or git fails: then nothing is
    /// downgraded).
    async fn target_delta(
        &self,
        repo: RepoId,
        mine: &ObjectId,
        theirs: &ObjectId,
        deadline: Instant,
        request_id: &RequestId,
    ) -> Option<BTreeSet<String>> {
        if mine == theirs {
            return None;
        }
        let timeout = Self::timeout(deadline, self.limits.read_timeout)?;
        match self.paths(repo, mine, theirs, timeout).await {
            Ok(paths) => paths,
            Err(e) => {
                tracing::warn!(
                    target: "forge_review",
                    event = "forge.review.conflict_delta_unread",
                    error_code = e.code().id(),
                    request_id = %request_id,
                    error = %e,
                    "could not read the target's changes between the bases; the conflict stands"
                );
                None
            }
        }
    }

    /// The git part (module docs, steps 2 and 3).
    #[allow(clippy::too_many_lines)] // one scan, read top to bottom
    async fn compute(
        &self,
        repo: RepoId,
        head: Option<&ObjectId>,
        target: &RefName,
        peers: &[Peer],
        total_peers: i64,
        request_id: &RequestId,
    ) -> Computed {
        let deadline = Instant::now() + self.limits.budget;
        let failed = |reason: &'static str, head: Option<&ObjectId>, tip: Option<&ObjectId>| {
            ScanResult {
                status: ScanStatus::Failed,
                reason: Some(reason),
                head: head.cloned(),           // clone: the result owns its ids
                target_branch: target.clone(), // clone: as above
                target_sha: tip.cloned(),      // clone: as above
                peers: 0,
                pairs_tried: 0,
                pairs_skipped: 0,
                pairs: Vec::new(),
            }
        };
        let done = |result| Computed {
            result,
            git_error: None,
        };
        let Some(head) = head else {
            return done(failed("no_version", None, None));
        };
        let Some(read) = Self::timeout(deadline, self.limits.read_timeout) else {
            return done(failed("budget_exhausted", Some(head), None));
        };
        let tip = match self.git.read_ref(&self.storage, repo, target, read).await {
            Ok(Some(tip)) => tip,
            Ok(None) => return done(failed("target_missing", Some(head), None)),
            Err(e) => {
                return Computed {
                    result: failed(git_reason(&e).as_str(), Some(head), None),
                    git_error: Some(e),
                }
            }
        };
        let (own_base, own) = match self.changed(repo, &tip, head, deadline).await {
            Ok(SideRead::Paths { base, paths }) => (base, paths),
            Ok(SideRead::Unrelated) => {
                return done(failed("unrelated_histories", Some(head), Some(&tip)))
            }
            Ok(SideRead::Budget) => {
                return done(failed("budget_exhausted", Some(head), Some(&tip)))
            }
            Err(e) => {
                return Computed {
                    result: failed(git_reason(&e).as_str(), Some(head), Some(&tip)),
                    git_error: Some(e),
                }
            }
        };
        let mut pairs = Vec::new();
        let mut candidates = Vec::new();
        // A failure on a pair is stored on the pair (review MEDIUM: not the
        // job's error; the sweep re-requests the scan).
        let mut pair_failed = false;
        let mut budget_out = false;
        let mut skipped = 0usize;
        for peer in peers {
            match self.changed(repo, &tip, &peer.head, deadline).await {
                Ok(SideRead::Budget) => {
                    budget_out = true;
                    skipped += 1;
                }
                Ok(SideRead::Unrelated) => pairs.push(pair_of(
                    peer,
                    PairOutcome::Failed,
                    Some("unrelated_histories"),
                )),
                Ok(SideRead::Paths { base, paths }) => {
                    if let (Some(mine), Some(theirs)) = (&own, &paths) {
                        if !overlaps(mine, theirs) {
                            pairs.push(pair_of(peer, PairOutcome::NoOverlap, None));
                            continue;
                        }
                    }
                    candidates.push(Candidate {
                        peer,
                        base,
                        theirs: paths,
                    });
                }
                Err(e) => {
                    log_pair_failure(request_id, peer, &e);
                    pairs.push(pair_of(
                        peer,
                        PairOutcome::Failed,
                        Some(git_reason(&e).as_str()),
                    ));
                    pair_failed = true;
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
            let Some(timeout) = Self::timeout(deadline, self.limits.merge_tree.timeout) else {
                budget_out = true;
                skipped += 1;
                continue;
            };
            let limits = MergeTreeLimits {
                timeout,
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
                    let delta = self
                        .target_delta(repo, &own_base, &candidate.base, deadline, request_id)
                        .await;
                    let count = files.len();
                    match verdict(
                        files,
                        files_truncated,
                        own.as_ref(),
                        candidate.theirs.as_ref(),
                        delta.as_ref(),
                    ) {
                        Verdict::TargetOnly => {
                            tracing::info!(
                                target: "forge_review",
                                event = "forge.review.conflict_downgraded",
                                request_id = %request_id,
                                other_merge_request_id = peer.merge_request.get(),
                                conflict_files = count,
                                "every conflicted path is one side's conflict with the target; the pair is clean"
                            );
                            pairs.push(pair_of(peer, PairOutcome::Clean, None));
                        }
                        Verdict::Conflict { kept } => {
                            let (conflicts, cut) = stored_files(&kept);
                            pairs.push(PairResult {
                                conflicts,
                                conflicts_truncated: files_truncated || cut,
                                ..pair_of(peer, PairOutcome::Conflict, None)
                            });
                        }
                    }
                }
                Err(e) => {
                    log_pair_failure(request_id, peer, &e);
                    pairs.push(pair_of(
                        peer,
                        PairOutcome::Failed,
                        Some(git_reason(&e).as_str()),
                    ));
                    pair_failed = true;
                }
            }
        }
        let peers_capped = i64::try_from(peers.len()).unwrap_or(i64::MAX) < total_peers;
        let reason = if pair_failed {
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
        done(ScanResult {
            status: if reason.is_some() {
                ScanStatus::Incomplete
            } else {
                ScanStatus::Scanned
            },
            reason,
            head: Some(head.clone()),      // clone: the result owns its ids
            target_branch: target.clone(), // clone: as above
            target_sha: Some(tip),
            peers: i32_of(peers.len()),
            pairs_tried: i32_of(tried),
            pairs_skipped: i32_of(skipped),
            pairs,
        })
    }
}

/// A git failure on one pair: logged once with its code (the pair stores
/// the reason; the scan is `incomplete pair_failed`).
fn log_pair_failure(request_id: &RequestId, peer: &Peer, e: &ForgeError) {
    tracing::warn!(
        target: "forge_review",
        event = "forge.review.conflict_pair_failed",
        error_code = e.code().id(),
        request_id = %request_id,
        other_merge_request_id = peer.merge_request.get(),
        error = %e,
        "git failed on one pair of the conflict scan; stored on the pair"
    );
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
        // Never left `scanning` (review LOW); a reopen requests a new scan.
        review::abandon_scan(db, mr, seq, "not_open").await?;
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
    let found = if head.is_some() {
        review::scan_peers(db, stored.project_id, &stored.target_branch, mr, limit).await?
    } else {
        review::Peers {
            peers: Vec::new(),
            total: 0,
            invalid: Vec::new(),
        }
    };
    for other in &found.invalid {
        // Never written by forge: that pair is left out (review LOW), not
        // the whole scan.
        tracing::warn!(
            target: "forge_review",
            event = "forge.review.conflict_peer_invalid",
            error_code = ErrorCode::Internal.id(),
            request_id = %request_id,
            merge_request_id = mr.get(),
            other_merge_request_id = other.get(),
            reason = "stored_head_sha_invalid",
            "a peer's stored head is not an object id; its pair is not scanned"
        );
    }
    let Computed {
        mut result,
        git_error,
    } = predictor
        .compute(
            target.repository,
            head.as_ref(),
            &stored.target_branch,
            &found.peers,
            found.total,
            request_id,
        )
        .await;
    if !found.invalid.is_empty() && result.status == ScanStatus::Scanned {
        result.status = ScanStatus::Incomplete;
        result.reason = Some("peer_invalid");
    }
    result.pairs_skipped = result
        .pairs_skipped
        .saturating_add(i32_of(found.invalid.len()));
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
        ScanRecorded::Stored {
            predicted,
            status,
            reason,
            skipped,
        } => {
            // What was stored (a peer that moved makes it `incomplete`).
            result.status = status;
            result.reason = reason;
            result.pairs_skipped = result.pairs_skipped.saturating_add(i32_of(skipped));
            ("scanned", predicted)
        }
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
            error_code = review::prediction_incomplete("failed").code().id(),
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

/// What the version job's scan request did (its log value and the seq).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanAsked {
    /// `requested`, `answered`, `not_open`, `no_head` or `failed`.
    pub outcome: &'static str,
    pub seq: Option<i64>,
}

/// The version job's request after a version: a scan of `mr` at its
/// current head unless its stored scan already answers it (see
/// `forge_db::review::request_scan_for_head`; nothing before the first
/// version or when the merge request is not open). **Never fails the version
/// job** (review HIGH): a failure is logged here at warn with its code and
/// request id (`outcome = failed`), and the worker's sweep requests the scan
/// later (`never_scanned`, or the retryable reason of the last scan).
pub(crate) async fn after_version(
    db: &Db,
    mr: MergeRequestId,
    request_id: &RequestId,
) -> ScanAsked {
    match ask_after_version(db, mr, request_id).await {
        Ok(asked) => asked,
        Err(e) => {
            tracing::warn!(
                target: "forge_review",
                event = "forge.review.conflict_scan_request_failed",
                error_code = e.code().id(),
                request_id = %request_id,
                merge_request_id = mr.get(),
                error = %e,
                "could not request the conflict scan after a version; the sweep requests it"
            );
            ScanAsked {
                outcome: "failed",
                seq: None,
            }
        }
    }
}

async fn ask_after_version(
    db: &Db,
    mr: MergeRequestId,
    request_id: &RequestId,
) -> Result<ScanAsked, ForgeError> {
    let no = |outcome| ScanAsked { outcome, seq: None };
    let Some(now) = review::refresh_target(db, mr).await? else {
        return Ok(no("not_open"));
    };
    let Some(head) = now
        .merge_request
        .head_sha
        .as_deref()
        .map(ObjectId::parse)
        .transpose()?
    else {
        return Ok(no("no_head"));
    };
    // clone: the job keeps its origin
    let asked = review::request_scan_for_head(db, mr, &head, request_id.clone()).await?;
    Ok(ScanAsked {
        outcome: asked.as_str(),
        seq: asked.seq(),
    })
}

#[cfg(test)]
#[path = "conflicts_tests.rs"]
mod tests;
