//! The `merge-request-refresh` subscriber (M3 R1b, plan 11 §11.3): on
//! `repository.pushed` it ENQUEUES the version's job for every open merge
//! request whose source branch the push moved — since plan 18 S6 the
//! prepare job (`merge_request.prepare`, `super::prepare`: the version and
//! its checks in one job; the version job `merge_request.refresh` before);
//! the job reads the branch with git and writes the version (a subscriber
//! never calls git: external calls belong in jobs, ADR-0016 §7).
//!
//! - When the event lists every ref (`refs_truncated = false`), the merge
//!   requests are those whose `source_branch` is a listed ref that was
//!   created, fast-forwarded or forced (a deleted branch adds no version).
//! - When the list was cut (`refs_truncated = true`, more than
//!   `MAX_LISTED_REFS` refs), EVERY open merge request of the project is
//!   re-checked: nothing is missed silently (a job whose branch did not
//!   move writes nothing). One push enqueues at most
//!   [`MAX_REFRESH_FANOUT`] jobs (most recently updated first); more is
//!   logged at warn (`forge.review.refresh_fanout_capped`).
//!
//! Each job's dedup key is `mr_prepare:<merge_request_id>:push:<seq>`: a
//! retried step never doubles a job, and each push gets its own. The pusher
//! (`actor_user_id` of a `user` or `service_account` event) becomes the
//! version's `pushed_by`.
//!
//! **Target pushes** (M3 R2, ADR-0024 §3): every listed `refs/heads/*` that
//! was created, moved OR deleted is also a target: the open merge requests
//! targeting it get a mergeability check (`mergeability::request_check_on`:
//! `unchecked`, a new `check_seq`, the job `merge_request.mergeability`),
//! the most recently updated first, at most the configured fan-out cap
//! (`review.mergeability_fanout_max`); the rest are only marked `unchecked`
//! (their merge stays blocked, never "mergeable") and the overflow is
//! logged once at warn (`forge.review.mergeability_deferred`). A deleted
//! target is checked too: the job finds no tip and stores `check_failed
//! reason=target_missing`. A cut list (`refs_truncated`) names no target:
//! the version jobs it enqueues for every open merge request compare the
//! stored pair with the current one and request the checks that are stale.
//!
//! **Path filter** (plan 18 S4, ADR-0037 (c); `review.path_filter`): a
//! merge's event says what the landing changed (the payload's `landed`
//! block, forge-core `merge_paths::LandedPaths`). When that block is
//! complete, the decided results of the merge requests it cannot interact
//! with are **carried forward** to the new tip in one statement
//! (`mergeability::paths`), and only the others are requested. Every other
//! case is the behaviour above, unchanged: the filter is off, the event
//! has no block (a direct push, a payload written before the block
//! existed), the block is incomplete, of another rule version or larger
//! than its budget (`merge_paths::LANDED_BUDGET`, enforced on read), or
//! its branch is not one the event moved. One info line per filtered landing
//! (`forge.review.mergeability_carried`: `carried`, `settled`,
//! `rechecked`), one per landing that could not be filtered
//! (`forge.review.mergeability_paths_unused`, with the reason).
//!
//! `independent`: an event that keeps failing is quarantined (doctor's
//! `subscribers.dead_letters` alerts) instead of holding up every later
//! push.

use forge_core::ids::{MergeRequestId, UserId};
use forge_core::merge_paths::{Landed, LandedPaths};
use forge_core::{EventType, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use super::mergeability::{
    carry_forward_on, defer_checks_on, not_checked, request_check_on, settled_on,
};
use super::prepare::prepare_job;
use super::versions::{RefreshRequest, RefreshTrigger};
use crate::jobs::enqueue_on;
use crate::query_error;
use crate::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};

/// The most version jobs one push enqueues (the most recently updated open
/// merge requests); beyond it `forge.review.refresh_fanout_capped` is
/// logged at warn.
pub const MAX_REFRESH_FANOUT: i64 = 500;

/// The subscriber's name (its row in forge.event_subscribers).
pub const REFRESH_SUBSCRIBER: &str = "merge-request-refresh";

/// The subscriber; it holds the mergeability fan-out cap
/// (`review.mergeability_fanout_max`) and whether landings are filtered by
/// path (`review.path_filter`).
#[derive(Debug, Clone, Copy)]
pub struct MergeRequestRefresh {
    mergeability_fanout: u32,
    path_filter: bool,
}

impl Default for MergeRequestRefresh {
    fn default() -> Self {
        Self::new(forge_core::config::ReviewConfig::DEFAULT_MERGEABILITY_FANOUT_MAX)
    }
}

impl MergeRequestRefresh {
    /// At most `mergeability_fanout` (at least 1) mergeability checks per
    /// target push.
    pub fn new(mergeability_fanout: u32) -> Self {
        Self {
            mergeability_fanout: mergeability_fanout.max(1),
            path_filter: forge_core::config::ReviewConfig::DEFAULT_PATH_FILTER,
        }
    }

    /// Whether a landing re-checks only the merge requests its paths may
    /// interact with (`review.path_filter`; `false`: every one, as before
    /// plan 18 S4).
    #[must_use]
    pub fn with_path_filter(mut self, path_filter: bool) -> Self {
        self.path_filter = path_filter;
        self
    }

    /// The landed paths of `event` the filter may use for one of `targets`
    /// (module docs), or why not (`Err`: the log's `paths` value; `None`:
    /// nothing to say — the filter is off).
    fn usable_landing(
        &self,
        event: &SourceEvent,
        targets: &[String],
    ) -> Result<LandedPaths, Option<&'static str>> {
        if !self.path_filter {
            return Err(None);
        }
        match LandedPaths::read(&event.payload) {
            Landed::Absent => Err(Some(NO_LANDED_BLOCK)),
            Landed::Unusable(reason) => Err(Some(reason)),
            Landed::Usable(landed) => {
                if targets.iter().any(|t| t == landed.target.as_str()) {
                    Ok(landed)
                } else {
                    Err(Some("ref_not_moved"))
                }
            }
        }
    }
}

/// The log's `paths` value of an event without a landed block.
const NO_LANDED_BLOCK: &str = "absent";

fn bad_payload(event: &SourceEvent, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_event_payload")
        .with_context("event_type", event.event_type.clone()) // clone: the context owns it
        .with_context("field", field)
        .with_context("seq", event.seq)
}

/// The branches a push changed: `(moved, deleted)` — `None`: the list was
/// cut, every open merge request is re-checked by its version job.
pub(crate) type PushedBranches = Option<(Vec<String>, Vec<String>)>;

/// The branches a push created or moved, and those it deleted.
pub(crate) fn pushed_branches(event: &SourceEvent) -> Result<PushedBranches, ForgeError> {
    let payload = &event.payload;
    let truncated = payload
        .get("refs_truncated")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| bad_payload(event, "refs_truncated"))?;
    if truncated {
        return Ok(None);
    }
    let refs = payload
        .get("refs")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| bad_payload(event, "refs"))?;
    let mut moved = Vec::new();
    let mut deleted = Vec::new();
    for entry in refs {
        let name = entry
            .get("ref")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| bad_payload(event, "refs.ref"))?;
        let kind = entry
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| bad_payload(event, "refs.kind"))?;
        if !name.starts_with("refs/heads/") {
            continue;
        }
        if kind == "delete" {
            deleted.push(name.to_owned());
        } else {
            moved.push(name.to_owned());
        }
    }
    Ok(Some((moved, deleted)))
}

impl MergeRequestRefresh {
    async fn on_push(&self, event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        let project = event
            .project_id
            .ok_or_else(|| bad_payload(event, "project_id"))?;
        let pushed = pushed_branches(event)?;
        if let Some((moved, deleted)) = &pushed {
            let targets: Vec<String> = moved.iter().chain(deleted).cloned().collect();
            self.on_targets(event, tx, project, &targets).await?;
        }
        let moved = pushed.map(|(moved, _)| moved);
        Self::on_sources(event, tx, project, moved).await
    }

    /// The mergeability checks of the open merge requests targeting one of
    /// `targets` (module docs).
    async fn on_targets(
        &self,
        event: &SourceEvent,
        tx: &mut DerivedTx<'_>,
        project: forge_core::ProjectId,
        targets: &[String],
    ) -> Result<(), ForgeError> {
        if targets.is_empty() {
            return Ok(());
        }
        // Plan 18 S4: the results the landing cannot interact with are
        // kept (carried to the new tip, or already answering for it);
        // `kept` stays empty whenever the landing cannot be filtered, and
        // everything below is then what it was before the filter.
        let mut kept: Vec<i64> = Vec::new();
        let mut filtered = None;
        match self.usable_landing(event, targets) {
            Ok(landed) => {
                let carried = carry_forward_on(tx.conn(), project, &landed).await?;
                let settled =
                    settled_on(tx.conn(), project, landed.target.as_str(), &landed.new).await?;
                kept.extend(&carried);
                kept.extend(&settled);
                kept.sort_unstable();
                kept.dedup();
                filtered = Some((carried.len(), kept.len() - carried.len()));
            }
            Err(Some(NO_LANDED_BLOCK)) => {
                // A push to the branch, or a record written before the
                // landed paths (or by a server with the filter off): as
                // before plan 18 S4, every merge request is checked again.
                // The ordinary case of every push, so debug.
                tracing::debug!(
                    target: "forge_db",
                    event = "forge.review.mergeability_paths_unused",
                    request_id = %event.request_id,
                    project_id = project.get(),
                    source_seq = event.seq,
                    paths_complete = false,
                    paths = NO_LANDED_BLOCK,
                    "the event names no landed paths; every open merge request targeting the branch is checked again"
                );
            }
            Err(Some(reason)) => {
                // Never silent: this landing checks every merge request
                // again, and says why it could not be filtered.
                tracing::info!(
                    target: "forge_db",
                    event = "forge.review.mergeability_paths_unused",
                    request_id = %event.request_id,
                    project_id = project.get(),
                    source_seq = event.seq,
                    paths_complete = false,
                    paths = reason,
                    "the landed paths cannot be used; every open merge request targeting the branch is checked again"
                );
            }
            Err(None) => {}
        }
        let cap = i64::from(self.mergeability_fanout);
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM forge.merge_requests WHERE project_id = $1 AND state = 'open' \
             AND target_branch = ANY($2) AND id <> ALL($4) \
             ORDER BY updated_at DESC, id LIMIT $3",
        )
        .bind(project.get())
        .bind(targets)
        .bind(cap)
        .bind(&kept)
        .fetch_all(tx.conn())
        .await
        .map_err(|e| query_error("review.mergeability_targets", e))?;
        if let Some((carried, settled)) = filtered {
            tracing::info!(
                target: "forge_db",
                event = "forge.review.mergeability_carried",
                request_id = %event.request_id,
                project_id = project.get(),
                source_seq = event.seq,
                paths_complete = true,
                carried,
                settled,
                rechecked = ids.len(),
                "a landing's mergeability results were carried forward where its paths cannot interact"
            );
        }
        let origin = event.request_id().unwrap_or_else(RequestId::generate);
        for id in &ids {
            request_check_on(
                tx.conn(),
                MergeRequestId::new(*id),
                project,
                origin.clone(), // clone: each job keeps the origin request
                Some(event.seq),
            )
            .await?;
        }
        // The kept ones are neither requested nor marked `unchecked`.
        let mut handled = ids.clone(); // clone: `ids` is still the enqueued count below
        handled.extend(&kept);
        let (deferred, skipped) = defer_checks_on(tx.conn(), project, targets, &handled).await?;
        if deferred > 0 {
            // Never silent: they stay `unchecked` (merge blocked); the
            // worker's sweep requests them, the longest waiting first.
            tracing::warn!(
                target: "forge_db",
                event = "forge.review.mergeability_deferred",
                request_id = %event.request_id,
                project_id = project.get(),
                source_seq = event.seq,
                targets = ?targets,
                enqueued = ids.len(),
                deferred,
                "more open merge requests target the pushed branches than one push checks; the rest are unchecked and swept"
            );
        }
        if skipped > 0 {
            // Past even the deferral bound: their status is stale (readers
            // compare the checked target with the live tip; the sweep only
            // sees `unchecked` rows, so a push or retarget re-checks them).
            let e = not_checked("fanout_deferred");
            tracing::error!(
                target: "forge_db",
                event = "forge.review.mergeability_not_checked",
                error_code = e.code().id(),
                reason = "fanout_deferred",
                request_id = %event.request_id,
                project_id = project.get(),
                source_seq = event.seq,
                targets = ?targets,
                skipped,
                "too many open merge requests target the pushed branches to mark them all"
            );
        }
        Ok(())
    }

    /// The version jobs of the open merge requests whose source moved
    /// (`None`: every open one).
    async fn on_sources(
        event: &SourceEvent,
        tx: &mut DerivedTx<'_>,
        project: forge_core::ProjectId,
        moved: Option<Vec<String>>,
    ) -> Result<(), ForgeError> {
        if moved.as_ref().is_some_and(Vec::is_empty) {
            return Ok(());
        }
        let truncated = moved.is_none();
        let cap = usize::try_from(MAX_REFRESH_FANOUT).unwrap_or(usize::MAX);
        let mut ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM forge.merge_requests WHERE project_id = $1 AND state = 'open' \
             AND ($2::text[] IS NULL OR source_branch = ANY($2)) \
             ORDER BY updated_at DESC, id LIMIT $3",
        )
        .bind(project.get())
        .bind(moved)
        .bind(MAX_REFRESH_FANOUT + 1)
        .fetch_all(tx.conn())
        .await
        .map_err(|e| query_error("review.refresh_targets", e))?;
        if ids.len() > cap {
            ids.truncate(cap);
            // Never silent: the rest are refreshed by their next push, open
            // or reopen (ADR-0024, R1b notes).
            tracing::warn!(
                target: "forge_db",
                event = "forge.review.refresh_fanout_capped",
                request_id = %event.request_id,
                project_id = project.get(),
                source_seq = event.seq,
                refs_truncated = truncated,
                enqueued = ids.len(),
                "more open merge requests than one push re-checks; the most recently updated were enqueued"
            );
        }
        // The job order does not matter; keep it stable for the logs.
        ids.sort_unstable();
        // A person's, a bot's (`user`) or a service account's push (S0
        // review, M3 I1b: those are audited as `service_account`).
        let pushed_by = matches!(event.actor_kind.as_str(), "user" | "service_account")
            .then_some(event.actor_user_id)
            .flatten()
            .map(UserId::new);
        let origin = event.request_id().unwrap_or_else(RequestId::generate);
        for id in ids {
            let job = prepare_job(
                RefreshRequest {
                    merge_request: MergeRequestId::new(id),
                    pushed_by,
                },
                RefreshTrigger::Pushed { seq: event.seq },
                origin.clone(), // clone: each job keeps the origin request
                Some(event.seq),
            )?;
            // Deduped: a retried step already enqueued it.
            let _enqueued = enqueue_on(tx.conn(), &job).await?;
        }
        Ok(())
    }
}

impl Subscriber for MergeRequestRefresh {
    fn name(&self) -> &'static str {
        REFRESH_SUBSCRIBER
    }

    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            let Some(kind) = EventType::parse(&event.event_type) else {
                // A type this build does not know (a newer writer). This
                // subscriber is independent (failing would quarantine the
                // event, and it reacts to pushes only), so it goes on, but
                // never silently (I1b review).
                tracing::warn!(
                    target: "forge_db",
                    event = "forge.subscribers.unknown_event_type",
                    subscriber = REFRESH_SUBSCRIBER,
                    request_id = %event.request_id,
                    source_seq = event.seq,
                    event_type = %event.event_type,
                    "an event type this build does not know was passed over"
                );
                return Ok(());
            };
            match kind {
                EventType::RepositoryPushed => self.on_push(event, tx).await,
                EventType::ProjectCreated
                | EventType::RepositoryCreated
                | EventType::TokenCreated
                | EventType::TokenRevoked
                | EventType::MemberAdded
                | EventType::MemberRoleChanged
                | EventType::MemberRemoved
                | EventType::ProtectedRefCreated
                | EventType::ProtectedRefUpdated
                | EventType::ProtectedRefDeleted
                | EventType::ProjectVisibilityChanged
                | EventType::ProjectDeleted
                | EventType::ProjectRestored
                | EventType::ProjectPurged
                | EventType::ProjectMoved
                | EventType::GroupMoved
                | EventType::UserRenamed
                | EventType::MergeRequestOpened
                | EventType::MergeRequestUpdated
                | EventType::MergeRequestDraftChanged
                | EventType::MergeRequestClosed
                | EventType::MergeRequestReopened
                | EventType::MergeRequestPushed
                | EventType::MergeRequestConflictDetected
                | EventType::MergeRequestConflictResolved
                | EventType::MergeRequestConflictPredicted
                | EventType::MergeRequestCommented
                | EventType::MergeRequestMerged
                | EventType::MergeRequestSourceBranchDeleted
                | EventType::MergeRequestApproved
                | EventType::MergeRequestUnapproved
                | EventType::CodeownersEnforcementChanged
                | EventType::CodeownersEnforcementRestored
                | EventType::CheckCompleted
                | EventType::WorkItemLinked
                | EventType::WorkItemUnlinked
                | EventType::MergeRequestWorkItemsChanged
                | EventType::MergeRequestRelinkRequested
                | EventType::WebhookDisabled
                | EventType::SpecReleased
                | EventType::MergeRequestRevertDrafted
                | EventType::MergeRequestAgentUpdateOpened
                | EventType::MergeRequestAiDraftProposed
                | EventType::MergeRequestAutofixFinished
                | EventType::DeploymentApprovalSummarized => Ok(()),
            }
        })
    }
}
