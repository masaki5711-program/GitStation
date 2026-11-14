//! The auto-merge worker (M4 C9; plan 04 M-5 / M-6; ADR-0017 Amendment
//! 2026-11-13): `forge serve` ticks [`AutoMerger`], which looks at the open
//! merge requests of projects whose `merge.auto` acts (`conditional` /
//! `full`; a project without a row is `full`) and, as the auto-merge
//! principal, adds each mergeable one to the merge queue (C8). The queue
//! tests it on the merge result with the merge requests ahead of it and
//! lands it; the landing decides again for the auto-merge principal.
//!
//! What it decides is `forge_core::auto_merge::decide` on facts read here:
//! the mode and the emergency stops, the queue, the `forge/risk` run on the
//! head (its `stop` rules), the merge request's own `ci/pipeline` on its
//! merge result, `evaluate_merge()`'s reasons (the merge status, with
//! `ci/pipeline` left to the queue) and, for `conditional`, the changed
//! paths. It stops **only** on the stop classes and the emergency stop;
//! everything else is a wait whose reason is shown. Each verdict is stored
//! (`forge.auto_merge_states`) as the merge request's one line; a new stop
//! is logged once (`FG-REVIEW-0026` / `FG-REVIEW-0027`).

use std::time::Duration;

use forge_core::auto_merge::{
    decide, deterministic_layer, Blocker, Facts, PipelineState, QueueFacts, StopClass, Verdict,
};
use forge_core::merge_results::reported_on_merge_results;
use forge_core::{
    CheckConclusion, CheckName, Freshness, MergeRequestState, ObjectId, ProjectRepository,
    RepositoryState, RequestId,
};
use forge_db::review::auto_merge::{self as db, Candidate, NewState};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::review_inputs::ChangedPaths;

use super::evaluate::BlockReason;
use super::Merger;
use crate::Review;

/// The bounds of one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoMergeLimits {
    /// Merge requests looked at per tick.
    pub batch: i64,
    /// A merge request whose head did not change is looked at again after
    /// this long.
    pub recheck: Duration,
    /// The most changed paths read for `conditional` (more: protected).
    pub max_paths: usize,
}

impl Default for AutoMergeLimits {
    fn default() -> Self {
        Self {
            batch: 50,
            recheck: Duration::from_secs(20),
            max_paths: 5000,
        }
    }
}

/// What one tick did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoMergeReport {
    pub looked: usize,
    pub queued: usize,
    pub waiting: usize,
    pub stopped: usize,
    pub failed: usize,
}

/// The auto-merge worker (module docs).
#[derive(Debug, Clone)]
pub struct AutoMerger {
    review: Review,
    merger: Merger,
    limits: AutoMergeLimits,
}

/// `FG-REVIEW-0028`: deciding one merge request failed (logged by the
/// tick with the cause's own code as `cause_code`).
pub fn scan_failed(cause: &ForgeError, c: &Candidate) -> ForgeError {
    ForgeError::new(ErrorCode::AutoMergeScanFailed)
        .with_context("cause_code", cause.code().id())
        .with_context("project_id", c.project.get())
        .with_context("merge_request_id", c.merge_request.get())
}

/// The stored error code of a verdict (the one line's code).
fn code_of(verdict: &Verdict) -> Option<ErrorCode> {
    match verdict {
        Verdict::Stopped(stop) if stop.class == StopClass::EmergencyStop => {
            Some(ErrorCode::AutoMergeEmergencyStopped)
        }
        Verdict::Stopped(_) => Some(ErrorCode::AutoMergeStopped),
        Verdict::Inactive
        | Verdict::Enqueue { .. }
        | Verdict::Queued { .. }
        | Verdict::Waiting { .. }
        | Verdict::Held { .. } => None,
    }
}

/// `evaluate_merge()`'s reasons as the auto-merge reads them: what
/// `ci/pipeline` waits for is the queue's (it tests the train), a red
/// required check is named.
fn blockers(reasons: &[BlockReason]) -> Vec<Blocker> {
    reasons
        .iter()
        .filter_map(|r| match r {
            BlockReason::RequiredCheckMissing { name }
            | BlockReason::RequiredCheckStale { name }
                if reported_on_merge_results(name) =>
            {
                None
            }
            BlockReason::RequiredCheckNotSuccessful {
                name, conclusion, ..
            } => {
                let red = matches!(
                    conclusion,
                    Some(CheckConclusion::Failure | CheckConclusion::Error)
                );
                if !red && reported_on_merge_results(name) {
                    return None;
                }
                Some(Blocker {
                    reason: r.as_str(),
                    red_check: red.then(|| name.as_str().to_owned()),
                })
            }
            // The merge request is in the queue or merging: not a wait.
            BlockReason::MergeInProgress => None,
            other @ (BlockReason::NotOpen
            | BlockReason::Draft
            | BlockReason::SourceBranchMissing
            | BlockReason::MergeabilityUnknown
            | BlockReason::Conflict
            | BlockReason::FfNotPossible
            | BlockReason::NothingToMerge
            | BlockReason::ApprovalsMissing { .. }
            | BlockReason::CodeownerApprovalMissing { .. }
            | BlockReason::CodeownersInvalid { .. }
            | BlockReason::ConfigChangeNeedsMaintainerApproval
            | BlockReason::ChangedPathsUnknown { .. }
            | BlockReason::RequiredCheckMissing { .. }
            | BlockReason::RequiredCheckStale { .. }
            | BlockReason::UnresolvedDiscussions { .. }
            | BlockReason::WorkItemLinkMissing) => Some(Blocker {
                reason: other.as_str(),
                red_check: None,
            }),
        })
        .collect()
}

impl AutoMerger {
    pub fn new(review: Review, merger: Merger) -> Self {
        Self {
            review,
            merger,
            limits: AutoMergeLimits::default(),
        }
    }

    #[must_use]
    pub fn with_limits(mut self, limits: AutoMergeLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> AutoMergeLimits {
        self.limits
    }

    /// Look at the due merge requests once (module docs). A failure of one
    /// merge request is logged (`FG-REVIEW-0028`) and counted; the others
    /// go on. A failure to list them is returned.
    pub async fn tick(&self) -> Result<AutoMergeReport, ForgeError> {
        let db = &self.merger.db;
        db::forget_inactive(db, 200).await?;
        let due = db::candidates(db, self.limits.batch, self.limits.recheck).await?;
        let mut report = AutoMergeReport::default();
        for c in &due {
            report.looked += 1;
            let request_id = RequestId::generate();
            match self.scan(c, &request_id).await {
                Ok(Verdict::Enqueue { .. } | Verdict::Queued { .. }) => report.queued += 1,
                Ok(Verdict::Stopped(_)) => report.stopped += 1,
                Ok(Verdict::Waiting { .. } | Verdict::Held { .. }) => report.waiting += 1,
                Ok(Verdict::Inactive) => {}
                Err(e) => {
                    report.failed += 1;
                    let failure = scan_failed(&e, c);
                    tracing::error!(
                        target: "forge_review",
                        event = "forge.auto_merge.scan_failed",
                        error_code = failure.code().id(),
                        cause_code = e.code().id(),
                        request_id = %request_id,
                        project_id = c.project.get(),
                        merge_request_id = c.merge_request.get(),
                        error = %e,
                        "auto-merge could not decide a merge request (runbook auto-merge)"
                    );
                }
            }
        }
        Ok(report)
    }

    /// Decide one merge request and record the verdict (module docs).
    pub async fn scan(&self, c: &Candidate, request_id: &RequestId) -> Result<Verdict, ForgeError> {
        let db = &self.merger.db;
        let settings = db::settings(db, c.project).await?;
        let Some(target) = forge_db::review::refresh_target(db, c.merge_request).await? else {
            db::forget(db, c.merge_request).await?;
            return Ok(Verdict::Inactive);
        };
        let mr = target.merge_request;
        let head = mr.head_sha.as_deref().map(ObjectId::parse).transpose()?;
        let (true, Some(head)) = (
            mr.state == MergeRequestState::Open && settings.mode.acts(),
            head,
        ) else {
            db::forget(db, mr.id).await?;
            return Ok(Verdict::Inactive);
        };
        let mut facts = Facts {
            mode: settings.mode,
            stop: settings.stop(),
            draft: mr.draft,
            queue: self.queue_facts(&mr, &head).await?,
            risk: forge_core::auto_merge::RiskVerdict::Pending,
            pipeline: PipelineState::None,
            blockers: Vec::new(),
            protected_path: false,
        };
        // Cheap verdicts first: no git, no check reads.
        let early = decide(&facts);
        let needs_more = matches!(
            early,
            Verdict::Waiting {
                reason: "risk_pending"
            }
        );
        let verdict = if !needs_more || !target.repository_ready {
            if needs_more {
                Verdict::Waiting {
                    reason: "repository_not_ready",
                }
            } else {
                early
            }
        } else {
            let repo = ProjectRepository {
                id: target.repository,
                state: RepositoryState::Ready,
            };
            // C9 review: decided before anything of the repository is read
            // (the mode, the stop, the project and the author's role).
            let proof = match super::queue::auto_proof(
                db,
                &mr,
                &settings,
                forge_auth::Action::MergeQueueEnqueue,
            )
            .await
            {
                Ok(proof) => proof,
                Err(e) if e.code() == ErrorCode::PendingDeletion => {
                    db::forget(db, mr.id).await?;
                    return Ok(Verdict::Inactive);
                }
                Err(e) if super::execute::is_refusal(&e) => {
                    let verdict = Verdict::Waiting {
                        reason: waiting_reason(&e),
                    };
                    self.record(&mr, &head, &verdict, request_id).await?;
                    return Ok(verdict);
                }
                Err(e) => return Err(e),
            };
            self.read_facts(&mut facts, &mr, &head, repo, request_id)
                .await?;
            match decide(&facts) {
                Verdict::Enqueue { warned } => {
                    match self
                        .review
                        .enqueue_auto(request_id, &proof, repo, (&mr, &head))
                        .await
                    {
                        Ok(_) => Verdict::Enqueue { warned },
                        // Decided again under the locks: what changed is a
                        // wait (a stop here would be a defect upstream).
                        Err(e) if super::execute::is_refusal(&e) => Verdict::Waiting {
                            reason: waiting_reason(&e),
                        },
                        Err(e) => return Err(e),
                    }
                }
                other @ (Verdict::Inactive
                | Verdict::Queued { .. }
                | Verdict::Waiting { .. }
                | Verdict::Held { .. }
                | Verdict::Stopped(_)) => other,
            }
        };
        self.record(&mr, &head, &verdict, request_id).await?;
        Ok(verdict)
    }

    async fn queue_facts(
        &self,
        mr: &forge_db::review::StoredMergeRequest,
        head: &ObjectId,
    ) -> Result<QueueFacts, ForgeError> {
        let entry = forge_db::review::merge_queue::entry_of(&self.merger.db, mr.id).await?;
        Ok(match entry {
            Some((e, _)) if e.state.is_live() => QueueFacts::Live {
                auto: e.credential.is_auto_merge(),
            },
            Some((e, _)) if e.credential.is_auto_merge() && &e.head_sha == head => {
                QueueFacts::Removed {
                    reason: e.reason.unwrap_or_default(),
                }
            }
            Some(_) | None => QueueFacts::None,
        })
    }

    /// The risk layers, the merge request's own pipeline, the merge
    /// status and (for `conditional`) the changed paths.
    async fn read_facts(
        &self,
        facts: &mut Facts,
        mr: &forge_db::review::StoredMergeRequest,
        head: &ObjectId,
        repo: ProjectRepository,
        request_id: &RequestId,
    ) -> Result<(), ForgeError> {
        let db = &self.merger.db;
        let project = mr.project_id;
        let risk_name = check_name(forge_core::checks::RISK_CHECK)?;
        let runs = forge_db::checks::runs_for(db, project, head).await?;
        let summary = db::risk_summary(db, project, head).await?;
        // The AI layer (plan 04 M-6 layer 2) lands in M5: `None` here; it
        // can only ever raise what the rules found (`combine`).
        facts.risk = forge_core::auto_merge::combine(
            deterministic_layer(runs.run(head, &risk_name), summary.as_deref()),
            None,
        );
        let target_tip = self
            .review
            .branch_tip(project, repo, mr.author, &mr.target_branch)
            .await?;
        let pipeline_name = check_name(forge_core::checks::CI_PIPELINE_CHECK)?;
        let merge_runs = self
            .review
            .checks
            .merge_runs_on(project, mr.id, head)
            .await?;
        facts.pipeline = PipelineState::of(
            merge_runs
                .resolve(head, target_tip.as_ref(), &pipeline_name)
                .filter(|(_, fresh)| *fresh == Freshness::Fresh)
                .map(|(run, _)| run),
        );
        let status = self
            .review
            .status_of(request_id, repo, mr.author, mr.clone(), None) // clone: the status owns its row
            .await?;
        facts.blockers = blockers(&status.reasons);
        if facts.mode == forge_core::auto_merge::AutoMergeMode::Conditional {
            facts.protected_path = self
                .touches_protected_path(repo, head, target_tip.as_ref())
                .await?;
        }
        Ok(())
    }

    /// Whether the diff from the merge base with the target to `head`
    /// touches a path `conditional` keeps for a person (more paths than the
    /// bound, a path that is no UTF-8, or no merge base count as one).
    async fn touches_protected_path(
        &self,
        repo: ProjectRepository,
        head: &ObjectId,
        target_tip: Option<&ObjectId>,
    ) -> Result<bool, ForgeError> {
        let Some(tip) = target_tip else {
            return Ok(true);
        };
        let git = &self.merger.git;
        let read = self.merger.timeouts.read;
        let base = git
            .merge_base(&self.merger.storage, repo.id, tip, head, read)
            .await?;
        let Some(base) = base else {
            return Ok(true);
        };
        let paths = git
            .changed_paths(
                &self.merger.storage,
                repo.id,
                &base,
                head,
                self.limits.max_paths,
                read,
            )
            .await?;
        Ok(match paths {
            ChangedPaths::Complete(paths) => paths
                .iter()
                .any(|p| forge_core::auto_merge::is_protected_path(p)),
            ChangedPaths::TooMany | ChangedPaths::NonUtf8 => true,
        })
    }

    async fn record(
        &self,
        mr: &forge_db::review::StoredMergeRequest,
        head: &ObjectId,
        verdict: &Verdict,
        request_id: &RequestId,
    ) -> Result<(), ForgeError> {
        let db = &self.merger.db;
        let Some(state) = verdict.state() else {
            return db::forget(db, mr.id).await;
        };
        let (class, reason, detail) = match verdict {
            Verdict::Stopped(stop) => (Some(stop.class), stop.reason, stop.detail.as_deref()),
            Verdict::Enqueue { warned: true } => (None, "queued_with_warning", None),
            Verdict::Enqueue { warned: false } => (None, "queued", None),
            Verdict::Queued { auto: true } => (None, "queued", None),
            Verdict::Queued { auto: false } => (None, "queued_by_person", None),
            Verdict::Waiting { reason } | Verdict::Held { reason } => (None, *reason, None),
            Verdict::Inactive => return Ok(()),
        };
        let code = code_of(verdict);
        let changed = db::record_state(
            db,
            &NewState {
                merge_request: mr.id,
                project: mr.project_id,
                head_sha: head,
                state,
                class,
                reason,
                detail,
                error_code: code,
                request_id,
            },
        )
        .await?;
        if changed {
            if let (Some(code), Verdict::Stopped(stop)) = (code, verdict) {
                tracing::info!(
                    target: "forge_review",
                    event = "forge.auto_merge.stopped",
                    error_code = code.id(),
                    request_id = %request_id,
                    project_id = mr.project_id.get(),
                    merge_request_id = mr.id.get(),
                    head_sha = head.as_str(),
                    class = stop.class.as_str(),
                    reason = stop.reason,
                    detail = stop.detail.as_deref(),
                    "auto-merge stopped for a merge request"
                );
            }
        }
        Ok(())
    }
}

fn check_name(raw: &str) -> Result<CheckName, ForgeError> {
    CheckName::parse(raw).map_err(|_| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "auto_merge_check_name")
            .with_context("name", raw.to_owned())
    })
}

/// The stable word of a refusal at the enqueue (its `reason` when it is
/// one of the merge's own words, else `refused`).
fn waiting_reason(e: &ForgeError) -> &'static str {
    let reason = e
        .context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    match (e.code(), reason) {
        (ErrorCode::MergeQueueEntryRefused, "already_queued") => "queued_by_person",
        (ErrorCode::HeadChanged, _) => "head_changed",
        (ErrorCode::MergeInProgress, _) => "merge_in_progress",
        (ErrorCode::RequiredCheckStale, _) => "required_check_stale",
        (ErrorCode::MergeBlocked, "approvals_missing") => "approvals_missing",
        (ErrorCode::MergeBlocked, "conflict") => "conflict",
        (ErrorCode::MergeBlocked, "mergeability_unknown") => "mergeability_unknown",
        (ErrorCode::MergeBlocked, _) => "merge_blocked",
        (ErrorCode::Forbidden, "auto_merge_emergency_stop") => "emergency_stop",
        (ErrorCode::Forbidden, "author_role") => "author_role",
        (ErrorCode::AccountDisabled, _) => "author_disabled",
        (ErrorCode::AutoMergeStopped, _) => "risk_changed",
        _ => "refused",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> CheckName {
        CheckName::parse(s).expect("name")
    }

    #[test]
    fn the_queue_proves_ci_pipeline_and_red_checks_are_named() {
        let reasons = vec![
            BlockReason::RequiredCheckMissing {
                name: name("ci/pipeline"),
            },
            BlockReason::RequiredCheckNotSuccessful {
                name: name("ci/pipeline"),
                status: forge_core::CheckStatus::InProgress,
                conclusion: None,
            },
            BlockReason::RequiredCheckNotSuccessful {
                name: name("ext/build"),
                status: forge_core::CheckStatus::Completed,
                conclusion: Some(CheckConclusion::Failure),
            },
            BlockReason::ApprovalsMissing {
                rule: "default".to_owned(),
                required: 1,
                counted: 0,
            },
        ];
        let got = blockers(&reasons);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].red_check.as_deref(), Some("ext/build"));
        assert_eq!(got[1].reason, "approvals_missing");
        // ci/pipeline red on the merge result is a red check like any.
        let red = blockers(&[BlockReason::RequiredCheckNotSuccessful {
            name: name("ci/pipeline"),
            status: forge_core::CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
        }]);
        assert_eq!(red[0].red_check.as_deref(), Some("ci/pipeline"));
    }

    /// FG-REVIEW-0028: a scan failure carries its cause's code.
    #[test]
    fn a_scan_failure_is_logged_with_its_cause() {
        let c = Candidate {
            merge_request: forge_core::MergeRequestId::new(5),
            project: forge_core::ProjectId::new(2),
            iid: 1,
        };
        let e = scan_failed(&ForgeError::new(ErrorCode::DbUnavailable), &c);
        assert_eq!(e.code(), ErrorCode::AutoMergeScanFailed);
        let ctx: Vec<(&str, &str)> = e.context().iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(ctx.contains(&("cause_code", ErrorCode::DbUnavailable.id())));
        assert!(ctx.contains(&("merge_request_id", "5")));
    }

    #[test]
    fn stops_carry_their_codes_and_waits_none() {
        let stop = Verdict::Stopped(forge_core::auto_merge::Stop {
            class: StopClass::Leak,
            reason: "risk_stop",
            detail: None,
        });
        assert_eq!(code_of(&stop), Some(ErrorCode::AutoMergeStopped));
        let emergency = Verdict::Stopped(forge_core::auto_merge::Stop {
            class: StopClass::EmergencyStop,
            reason: "project_stopped",
            detail: None,
        });
        assert_eq!(
            code_of(&emergency),
            Some(ErrorCode::AutoMergeEmergencyStopped)
        );
        assert_eq!(code_of(&Verdict::Waiting { reason: "draft" }), None);
        let refused = ForgeError::new(ErrorCode::MergeBlocked).with_context("reason", "conflict");
        assert_eq!(waiting_reason(&refused), "conflict");
    }
}
