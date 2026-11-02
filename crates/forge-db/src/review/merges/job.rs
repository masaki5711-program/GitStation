//! The merge job's SQL (ADR-0024 §4.2 steps 3, 5, 6, 9, 10 and the
//! reconciliation). Every state change is a conditional update on the
//! attempt's current state, so two workers (or a worker and the
//! reconciliation) can never both move one attempt.

use std::time::Duration;

use forge_core::ids::ProjectId;
use forge_core::{
    AuditDetails, DedupKey, DedupPart, MergeRequestChange, MergeRequestState, MergeState, ObjectId,
    PushRecord, PushedRef, RefName, SourceOutcome,
};
use forge_errors::{ErrorCode, ForgeError};

use super::request::{gate, MergeGate};
use super::{columns, MergeRow, StoredMerge};
use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome, Settle,
};
use crate::projects::lock_access;
use crate::review::commands::{absent, facts};
use crate::review::{Row, StoredMergeRequest, COLUMNS};
use crate::{query_error, Db};

/// What a gate transaction does after `decide` allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateStep {
    /// tx1a: decide only (nothing written when allowed).
    Check,
    /// tx1b: `queued → prepared` with the target tip the trial merge used
    /// and the commit the target will move to.
    Prepare {
        expected_old: ObjectId,
        new_sha: ObjectId,
    },
    /// Decide on the facts only, whatever the attempt's state (the source
    /// branch deletion after `applied`).
    Facts,
}

/// What a gate transaction found.
#[derive(Debug)]
pub enum GateOutcome<P> {
    /// `decide` allowed; the step's write (if any) is committed.
    Decided(P),
    /// The attempt is no longer in the state the step needs (another worker
    /// or the reconciliation moved it): nothing was decided or written.
    Moved(MergeState),
}

/// The attempt `merge_id`, locked.
async fn attempt_on(
    conn: &mut sqlx::postgres::PgConnection,
    merge_id: i64,
) -> Result<Option<StoredMerge>, ForgeError> {
    let row: Option<MergeRow> = sqlx::query_as(&format!(
        "SELECT {} FROM forge.merge_request_merges WHERE id = $1 FOR UPDATE",
        columns()
    ))
    .bind(merge_id)
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("review.merge_attempt_lock", e))?;
    row.map(MergeRow::parse).transpose()
}

fn missing_attempt(merge_id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "merge_attempt")
        .with_context("merge_id", merge_id)
}

/// The `reason` a refusal is stored with: its own `reason` context when it
/// is a plain word, else `refused`.
fn reason_of(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone()) // clone: the row keeps its own copy
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 64
                && v.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                && v.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
        .unwrap_or_else(|| "refused".to_owned())
}

/// Lock the facts of attempt `merge_id` (the project, its merge request,
/// the requester and the attempt, in the review lock order) and run
/// `decide` on them; then apply `step` (module docs of `merges`). A
/// refusal (`decide`'s error) ends the attempt `superseded` with the
/// error's `reason` and code in the same transaction, and is returned.
pub async fn merge_gate_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    merge_id: i64,
    step: GateStep,
    decide: impl FnOnce(&MergeGate) -> Result<P, ForgeError>,
) -> Result<GateOutcome<P>, ForgeError> {
    const ACTION: &str = "merge_request.merge_gate";
    let attempt = super::merge_attempt(db, merge_id)
        .await?
        .ok_or_else(|| missing_attempt(merge_id))?;
    let iid = super::iid_of(db, attempt.merge_request).await?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, attempt.project_id, Some(iid), attempt.requested_by).await?;
        let locked = tx
            .step(|c| async move { attempt_on(c, merge_id).await })
            .await?
            .ok_or_else(|| missing_attempt(merge_id))?;
        let needs = match step {
            GateStep::Check | GateStep::Prepare { .. } => Some(MergeState::Queued),
            GateStep::Facts => None,
        };
        if needs.is_some_and(|s| s != locked.state) {
            return Ok(Settle::Discard(Err(locked.state)));
        }
        let gate = gate(
            &mut tx,
            facts,
            attempt.requested_by,
            attempt.credential,
            Some(locked),
        )
        .await?;
        let decided = match decide(&gate) {
            Ok(p) => p,
            Err(refusal) => {
                if needs.is_none() {
                    return Err(refusal);
                }
                let reason = reason_of(&refusal);
                let code = refusal.code().id();
                tx.step(|c| async move {
                    sqlx::query(
                        "UPDATE forge.merge_request_merges SET state = 'superseded', \
                         reason = $2, error_code = $3, finished_at = now(), updated_at = now() \
                         WHERE id = $1 AND state = 'queued'",
                    )
                    .bind(merge_id)
                    .bind(reason)
                    .bind(code)
                    .execute(c)
                    .await
                    .map_err(|e| query_error("review.merge_refused", e))
                })
                .await?;
                return Ok(Settle::CommitDerived(Ok(Err(refusal))));
            }
        };
        match step {
            GateStep::Check | GateStep::Facts => Ok(Settle::Discard(Ok(Ok(decided)))),
            GateStep::Prepare {
                expected_old,
                new_sha,
            } => {
                let moved = tx
                    .step(|c| async move {
                        sqlx::query(
                            "UPDATE forge.merge_request_merges SET state = 'prepared', \
                             expected_old = $2, new_sha = $3, updated_at = now() \
                             WHERE id = $1 AND state = 'queued'",
                        )
                        .bind(merge_id)
                        .bind(expected_old.as_str())
                        .bind(new_sha.as_str())
                        .execute(c)
                        .await
                        .map_err(|e| query_error("review.merge_prepare", e))
                    })
                    .await?
                    .rows_affected();
                if moved == 1 {
                    Ok(Settle::CommitDerived(Ok(Ok(decided))))
                } else {
                    Ok(Settle::Discard(Err(MergeState::Queued)))
                }
            }
        }
    }
    .await;
    match tx.settle(ACTION, outcome).await? {
        Ok(Ok(decided)) => Ok(GateOutcome::Decided(decided)),
        Ok(Err(refusal)) => Err(refusal),
        Err(state) => Ok(GateOutcome::Moved(state)),
    }
}

/// The fence's compare-and-swap (ADR-0024 §4.2 step 6 (b)): `prepared →
/// executing` with `executing_at = now()`. `false`: the attempt was not
/// `prepared` (another worker or the reconciliation took it) — the ref
/// must not move.
pub async fn fence_merge(db: &Db, merge_id: i64) -> Result<bool, ForgeError> {
    const QUERY: &str = "review.merge_fence";
    db.within(QUERY, async {
        let done = sqlx::query(
            "UPDATE forge.merge_request_merges SET state = 'executing', executing_at = now(), \
             updated_at = now() WHERE id = $1 AND state = 'prepared'",
        )
        .bind(merge_id)
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(done.rows_affected() == 1)
    })
    .await
}

/// How long attempt `merge_id` has been `executing` by the database's
/// clock (`None`: it is not executing) — step 6 (c) compares it with the
/// reconciliation's grace right before `update-ref`.
pub async fn executing_elapsed(db: &Db, merge_id: i64) -> Result<Option<Duration>, ForgeError> {
    const QUERY: &str = "review.merge_elapsed";
    let secs: Option<f64> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT EXTRACT(EPOCH FROM (now() - executing_at))::float8 \
                 FROM forge.merge_request_merges WHERE id = $1 AND state = 'executing'",
            )
            .bind(merge_id)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(secs.map(|s| Duration::from_secs_f64(s.max(0.0))))
}

/// End attempt `merge_id` as `to` (`superseded` or `failed`) if it is in
/// one of `from`; `reason` and `code` say why. `false`: it was not (someone
/// else ended or moved it).
pub async fn end_merge(
    db: &Db,
    merge_id: i64,
    from: &[MergeState],
    to: MergeState,
    reason: &str,
    code: Option<ErrorCode>,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "review.merge_end";
    if !matches!(to, MergeState::Superseded | MergeState::Failed) {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "merge_end_not_final")
            .with_context("state", to.as_str()));
    }
    let from: Vec<&str> = from.iter().map(|s| s.as_str()).collect();
    db.within(QUERY, async {
        let done = sqlx::query(
            "UPDATE forge.merge_request_merges SET state = $3, reason = $4, error_code = $5, \
             finished_at = now(), updated_at = now() WHERE id = $1 AND state = ANY($2)",
        )
        .bind(merge_id)
        .bind(&from)
        .bind(to.as_str())
        .bind(reason)
        .bind(code.map(|c| c.id()))
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(done.rows_affected() == 1)
    })
    .await
}

/// What [`finish_merge`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeFinished {
    /// `executing → applied`, the merge request `merged`, the audit rows and
    /// events written.
    Recorded(Box<StoredMergeRequest>),
    /// An earlier run recorded it already: nothing written.
    AlreadyApplied,
    /// The attempt is no longer `executing` (and not `applied`): nothing
    /// written (the caller reports it, FG-REVIEW-0009 record_failed).
    NotExecuting(MergeState),
    /// The ref moved but the merge request is no longer open (a broken
    /// invariant: every transition of an open merge request refuses a live
    /// merge). The attempt was set `failed` (FG-REVIEW-0009
    /// `merge_request_not_open`) in this transaction — never left
    /// `executing` — for a person (runbook merge-execution).
    MergeRequestNotOpen(MergeRequestState),
}

fn merged_key(prefix: &'static str, merge_id: i64) -> Result<DedupKey, ForgeError> {
    DedupKey::new(prefix, &[DedupPart::Id(merge_id)])
}

/// The audit rows and events of one change on the project: the merge
/// request's (first) and the push's.
fn merge_record(
    project: ProjectId,
    change: MergeRequestChange,
    change_key: DedupKey,
    push: AuditDetails,
    push_key: DedupKey,
    repo_id: i64,
) -> Result<ChangeRecord, ForgeError> {
    let merge_request = change.merge_request();
    let details = AuditDetails::MergeRequest(change);
    let event_type = details.event_type().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "audit_only_event")
    })?;
    let push_type = push.event_type().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "audit_only_event")
    })?;
    let value = details.to_value();
    let entry = |action, details| AuditEntry {
        action,
        outcome: Outcome::Succeeded,
        resource_type: "project",
        resource_id: Some(project.get()),
        project_id: Some(project),
        error_code: None,
        details,
    };
    Ok(ChangeRecord {
        audit: entry(details.action(), value.clone()), // clone: the same JSON is the payload
        affected: vec![entry(push.action(), push.to_value())],
        events: vec![
            DomainEvent {
                event_type,
                schema_version: 1,
                aggregate_type: "merge_request",
                aggregate_id: merge_request.get(),
                project_id: Some(project),
                payload: value,
                dedup_key: Some(change_key),
            },
            DomainEvent {
                event_type: push_type,
                schema_version: 1,
                aggregate_type: "repository",
                aggregate_id: repo_id,
                project_id: Some(project),
                payload: push.event_payload(),
                dedup_key: Some(push_key),
            },
        ],
    })
}

/// tx2 (ADR-0024 §4.2 step 9): `executing → applied`, the merge request
/// `merged`, the `merge_request.merged` audit row on the project with the
/// target's `repository.pushed` row, and both events — deduplicated by the
/// merge id (`merge_request_merged:<merge_id>`, `merge_pushed:<merge_id>`)
/// so a retry or the reconciliation writes them once. `ctx` is the
/// requester's (the job acts for them). `push` is the target's update.
/// `range_pushers`: the pushers of the merged range (forge-review), who
/// with the requester get the merge or squash commit's `push_commits` rows;
/// `None` (unknown) writes no row (R5 review round 5, fail closed).
pub async fn finish_merge(
    db: &Db,
    ctx: &ChangeContext,
    attempt: &StoredMerge,
    push: &PushRecord,
    range_pushers: Option<&[forge_core::UserId]>,
) -> Result<MergeFinished, ForgeError> {
    const ACTION: &str = "merge_request.merged";
    push.check()?;
    let merge_id = attempt.id;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        lock_access(&mut tx, attempt.project_id).await?;
        let mr = attempt.merge_request;
        let current: Row = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "SELECT {COLUMNS} FROM forge.merge_requests WHERE id = $1 FOR UPDATE"
                ))
                .bind(mr.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("review.merge_finish_mr", e))
            })
            .await?
            .ok_or_else(absent)?;
        let current = current.parse()?;
        let locked = tx
            .step(|c| async move { attempt_on(c, merge_id).await })
            .await?
            .ok_or_else(|| missing_attempt(merge_id))?;
        match locked.state {
            MergeState::Executing => {}
            MergeState::Applied => return Ok(Settle::Discard(MergeFinished::AlreadyApplied)),
            other @ (MergeState::Queued
            | MergeState::Prepared
            | MergeState::Superseded
            | MergeState::Failed) => {
                return Ok(Settle::Discard(MergeFinished::NotExecuting(other)))
            }
        }
        if current.state != MergeRequestState::Open {
            tx.step(|c| async move {
                sqlx::query(
                    "UPDATE forge.merge_request_merges SET state = 'failed', \
                     reason = 'merge_request_not_open', error_code = $2, \
                     finished_at = now(), updated_at = now() \
                     WHERE id = $1 AND state = 'executing'",
                )
                .bind(merge_id)
                .bind(ErrorCode::MergeExecutionFailed.id())
                .execute(c)
                .await
                .map_err(|e| query_error("review.merge_not_open", e))
            })
            .await?;
            return Ok(Settle::CommitDerived(MergeFinished::MergeRequestNotOpen(
                current.state,
            )));
        }
        let (Some(expected_old), Some(new_sha)) =
            (locked.expected_old.clone(), locked.new_sha.clone())
        else {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "merge_finish_unprepared")
                .with_context("merge_id", merge_id));
        };
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.merge_request_merges SET state = 'applied', finished_at = now(), \
                 updated_at = now() WHERE id = $1 AND state = 'executing'",
            )
            .bind(merge_id)
            .execute(c)
            .await
            .map_err(|e| query_error("review.merge_applied", e))
        })
        .await?;
        let row: Row = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "UPDATE forge.merge_requests SET state = 'merged', revision = revision + 1, \
                     updated_at = now() WHERE id = $1 RETURNING {COLUMNS}"
                ))
                .bind(mr.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("review.merge_mr_merged", e))
            })
            .await?;
        let stored = row.parse()?;
        let change = MergeRequestChange::Merged {
            merge_request: mr,
            iid: stored.iid,
            merge_id,
            method: locked.method,
            target_branch: locked.target_branch.clone(), // clone: the change owns its name
            target_sha: expected_old,
            merged_sha: new_sha.clone(), // clone: the push commits row below needs it too
            head_sha: locked.head_sha.clone(), // clone: the change owns its id
            merged_by: locked.requested_by,
        };
        // M3 R5 round 4 / review round 5 (ADR-0024 §8.3): a merge or squash
        // commit is a commit this "push" introduced — by the requester and by
        // every pusher of the merged range (a squash must not launder them);
        // unknown pushers: no row at all (a later range holding it has
        // unknown committers).
        if let (true, Some(range_pushers)) = (
            locked.method != forge_core::MergeMethod::FastForward,
            range_pushers,
        ) {
            let mut users: Vec<forge_core::UserId> = range_pushers.to_vec();
            users.push(locked.requested_by);
            users.sort_unstable();
            users.dedup();
            let (project, repo) = (attempt.project_id, push.repository_id);
            let request_id = ctx.request_id.as_str().to_owned();
            let merged = std::slice::from_ref(&new_sha);
            for user in users {
                let request_id = request_id.as_str();
                tx.step(|c| async move {
                    crate::repositories::insert_push_commits(
                        c,
                        (project, repo),
                        request_id,
                        user,
                        merged,
                    )
                    .await
                })
                .await?;
            }
        }
        let pushed = AuditDetails::RepositoryPushed {
            project_id: attempt.project_id,
            repository_id: push.repository_id,
            updates: push.sorted_updates(),
            replayed_from: None,
        };
        let record = merge_record(
            attempt.project_id,
            change,
            merged_key("merge_request_merged", merge_id)?,
            pushed,
            merged_key("merge_pushed", merge_id)?,
            push.repository_id.get(),
        )?;
        Ok(Settle::Commit(
            record,
            MergeFinished::Recorded(Box::new(stored)),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// The source branch a merge deleted (ADR-0024 §4.2 step 10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDeletion {
    pub repo: forge_core::RepoId,
    pub source_branch: RefName,
    /// The commit the branch pointed at.
    pub old: ObjectId,
}

/// Record a deleted source branch in its own transaction (ADR-0024 §4.2
/// step 10): `source_outcome = deleted` on the attempt, the
/// `merge_request.source_branch_deleted` audit row on the project with the
/// delete's `repository.pushed` row, and both events (dedup
/// `merge_source_deleted:<merge_id>`, `merge_source_pushed:<merge_id>`).
/// `ctx` is the requester's.
pub async fn record_source_deleted(
    db: &Db,
    ctx: &ChangeContext,
    attempt: &StoredMerge,
    iid: i64,
    deletion: &SourceDeletion,
) -> Result<(), ForgeError> {
    const ACTION: &str = "merge_request.source_branch_deleted";
    let width = deletion.old.as_str().len();
    let zero = ObjectId::parse(&"0".repeat(width))?;
    let push = PushRecord {
        repository_id: deletion.repo,
        updates: vec![PushedRef {
            name: deletion.source_branch.clone(), // clone: the record owns its name
            old: deletion.old.clone(),
            new: zero,
            kind: forge_core::PushUpdateKind::Delete,
            commits: Vec::new(),
            total_commits: 0,
            total_commits_partial: false,
        }],
    };
    push.check()?;
    let merge_id = attempt.id;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        lock_access(&mut tx, attempt.project_id).await?;
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.merge_request_merges SET source_outcome = 'deleted', \
                 updated_at = now() WHERE id = $1 AND state = 'applied' \
                 AND source_outcome IS NULL",
            )
            .bind(merge_id)
            .execute(c)
            .await
            .map_err(|e| query_error("review.merge_source_outcome", e))
        })
        .await?;
        let change = MergeRequestChange::SourceBranchDeleted {
            merge_request: attempt.merge_request,
            iid,
            merge_id,
            source_branch: deletion.source_branch.clone(), // clone: the change owns its name
            old: deletion.old.clone(),
        };
        let pushed = AuditDetails::RepositoryPushed {
            project_id: attempt.project_id,
            repository_id: deletion.repo,
            updates: push.sorted_updates(),
            replayed_from: None,
        };
        let record = merge_record(
            attempt.project_id,
            change,
            merged_key("merge_source_deleted", merge_id)?,
            pushed,
            merged_key("merge_source_pushed", merge_id)?,
            deletion.repo.get(),
        )?;
        Ok(Settle::Commit(record, ()))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Set what happened to the source branch when it was kept (once, after
/// `applied`; a deletion is recorded by [`record_source_deleted`]).
pub async fn set_source_outcome(
    db: &Db,
    merge_id: i64,
    outcome: SourceOutcome,
) -> Result<(), ForgeError> {
    const QUERY: &str = "review.merge_source_outcome";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.merge_request_merges SET source_outcome = $2, updated_at = now() \
             WHERE id = $1 AND state = 'applied' AND source_outcome IS NULL",
        )
        .bind(merge_id)
        .bind(outcome.as_str())
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(())
    })
    .await
}

/// Record that the reconciliation looked at live attempt `merge_id` without
/// settling it (busy lock, an error it logged): the next scan takes the
/// attempts it has not looked at for longest first (review round 1:
/// no head-of-line blocking).
pub async fn mark_reconcile_seen(db: &Db, merge_id: i64) -> Result<(), ForgeError> {
    const QUERY: &str = "review.merge_reconcile_seen";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.merge_request_merges SET reconcile_seen_at = now() \
             WHERE id = $1 AND state IN ('queued', 'prepared', 'executing')",
        )
        .bind(merge_id)
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(())
    })
    .await
}

/// The attempts the reconciliation looks at (ADR-0024 §4.2 照合), never
/// looked at (or looked at longest ago) first, at most `limit`: `queued` / `prepared` not updated for
/// `idle_after`, and `executing` for longer than `executing_after` (before
/// that, the worker's `update-ref` may still run).
pub async fn stuck_merges(
    db: &Db,
    idle_after: Duration,
    executing_after: Duration,
    limit: i64,
) -> Result<Vec<StoredMerge>, ForgeError> {
    const QUERY: &str = "review.merge_stuck";
    let rows: Vec<MergeRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_request_merges WHERE \
                 (state IN ('queued', 'prepared') \
                  AND updated_at < now() - make_interval(secs => $1)) \
                 OR (state = 'executing' AND executing_at < now() - make_interval(secs => $2)) \
                 ORDER BY reconcile_seen_at NULLS FIRST, id LIMIT $3",
                columns()
            ))
            .bind(idle_after.as_secs_f64())
            .bind(executing_after.as_secs_f64())
            .bind(limit)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(MergeRow::parse).collect()
}
