//! Plan 18 S5 (ADR-0037): "run the pipeline on the current target" — a
//! person's ask for the merge request pipeline (`ci/pipeline`) on the merge
//! result of the merge request's head into the target tip its stored
//! mergeability answers for. Since S5 a target move writes the new merge
//! result without a pipeline (`MergeRequestPipeline::OnNewHead`); this is
//! how a person gets one for a direct merge (the merge queue does not need
//! it: its train's pipeline gates the landing).
//!
//! One transaction, decided again on the locked project and caller
//! (`decide`: forge-pipelines reconfirms `authorize()` for `PipelineCreate`),
//! with the merge request's mergeability row locked:
//!
//! - a computed `can_be_merged` for the head with its merge result → the
//!   merge result's `ci.pipeline_create` job (deduplicated on the
//!   pipeline's identity): `queued` (one `pipeline.run_merge_request` audit
//!   row), or `exists` when its pipeline row exists or its job is still
//!   there (nothing written);
//! - no merge result for the tip yet (a carried row, unchecked, a failed
//!   check, another head) → a check is requested whose job carries the ask
//!   (`"pipeline": true`), and the pipeline follows its merge result:
//!   `computing` with the new `check_seq` (one audit row) — or, when such
//!   a job is already queued or running for the row's request, `computing`
//!   with that seq and nothing written (a double click asks once).
//!
//! Refusals (nothing written): no such merge request `FG-REPO-0005
//! resource=merge_request`; `FG-CI-0003 operation=run_merge_request` with
//! `reason=not_open` (closed or merged), `no_version` (nothing pushed yet)
//! or `conflict` (the head conflicts with the target: no merge result).

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::pipeline::PipelineChange;
use forge_core::{AuditDetails, ObjectId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::merge_results::enqueue_pipeline;
use super::mergeability::request_check_asking_on;
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::pipelines::decided_facts;
use crate::projects::ProjectUpdateFacts;
use crate::query_error;
use crate::Db;

/// What an ask did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergePipelineOutcome {
    /// The pipeline job of the current merge result was enqueued.
    Queued,
    /// It had been asked for already (its job exists): nothing written.
    Exists,
    /// No merge result for the tip yet: a check is on its way and the
    /// pipeline follows it.
    Computing,
}

impl MergePipelineOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Exists => "exists",
            Self::Computing => "computing",
        }
    }
}

/// The answer of [`run_merge_request_pipeline_decided`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePipelineRun {
    pub outcome: MergePipelineOutcome,
    pub merge_request: MergeRequestId,
    pub iid: i64,
    /// The merge result the pipeline runs on (`queued`, `exists`).
    pub merge_result: Option<ObjectId>,
    /// The target tip of that merge result (`queued`, `exists`).
    pub target: Option<ObjectId>,
    /// The check the pipeline waits for (`computing`).
    pub check_seq: Option<i64>,
}

fn conflict(reason: &'static str, project: ProjectId, iid: i64) -> ForgeError {
    ForgeError::new(ErrorCode::PipelineStateConflict)
        .with_context("operation", "run_merge_request")
        .with_context("reason", reason)
        .with_context("project_id", project.get())
        .with_context("iid", iid)
}

fn stored_invalid(mr: MergeRequestId, column: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "merge_pipeline_row_invalid")
        .with_context("merge_request_id", mr.get())
        .with_context("column", column)
}

fn oid(mr: MergeRequestId, column: &'static str, raw: &str) -> Result<ObjectId, ForgeError> {
    ObjectId::parse(raw).map_err(|_| stored_invalid(mr, column))
}

/// The merge request as the ask reads it.
struct Subject {
    mr: MergeRequestId,
    open: bool,
    head: Option<String>,
    source: String,
}

/// The mergeability row as the ask reads it (locked).
struct Row {
    status: String,
    check_seq: i64,
    checked_head: Option<String>,
    checked_target: Option<String>,
    /// The merge result of (checked head, checked target), if written.
    result: Option<String>,
    /// A queued or running job of the row's request carries an ask.
    asked: bool,
}

async fn subject_on(
    conn: &mut PgConnection,
    project: ProjectId,
    iid: i64,
) -> Result<Option<Subject>, ForgeError> {
    let row: Option<(i64, String, Option<String>, String)> = sqlx::query_as(
        "SELECT id, state, head_sha, source_branch FROM forge.merge_requests \
         WHERE project_id = $1 AND iid = $2",
    )
    .bind(project.get())
    .bind(iid)
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("review.merge_pipeline_subject", e))?;
    Ok(row.map(|(id, state, head, source)| Subject {
        mr: MergeRequestId::new(id),
        open: state == "open",
        head,
        source,
    }))
}

/// The columns of [`row_on`]'s statement.
type RowColumns = (
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
);

async fn row_on(conn: &mut PgConnection, mr: MergeRequestId) -> Result<Option<Row>, ForgeError> {
    let row: Option<RowColumns> = sqlx::query_as(
        "SELECT g.status, g.check_seq, g.checked_head_sha, g.checked_target_sha, \
             (SELECT r.result_sha FROM forge.merge_result_refs r \
              WHERE r.merge_request_id = g.merge_request_id AND r.head_sha = g.checked_head_sha \
              AND r.target_sha = g.checked_target_sha), \
             EXISTS (SELECT 1 FROM forge.jobs j WHERE j.kind = 'merge_request.mergeability' \
              AND j.dedup_key = 'mr_mergeability:' || g.merge_request_id || ':' || g.check_seq \
              AND j.state IN ('queued', 'running') AND j.payload ->> 'pipeline' = 'true') \
             FROM forge.merge_request_mergeability g WHERE g.merge_request_id = $1 \
             FOR UPDATE OF g",
    )
    .bind(mr.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("review.merge_pipeline_row", e))?;
    Ok(row.map(
        |(status, check_seq, checked_head, checked_target, result, asked)| Row {
            status,
            check_seq,
            checked_head,
            checked_target,
            result,
            asked,
        },
    ))
}

/// Whether the merge request pipeline of `result` exists (any status):
/// `ci_pipelines_once` (project, source, ref, sha, merge request) serves it.
async fn pipeline_exists(
    conn: &mut PgConnection,
    (project, mr): (ProjectId, MergeRequestId),
    source: &str,
    result: &ObjectId,
) -> Result<bool, ForgeError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM forge.ci_pipelines WHERE project_id = $1 \
         AND source = 'merge_request' AND ref = $2 AND sha = $3 \
         AND coalesce(merge_request_id, 0) = $4)",
    )
    .bind(project.get())
    .bind(source)
    .bind(result.as_str())
    .bind(mr.get())
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("review.merge_pipeline_exists", e))
}

/// What the locked facts call for.
enum Step {
    /// Enqueue the pipeline job of `result` (the merge of the head into
    /// `target`).
    Enqueue { result: ObjectId, target: ObjectId },
    /// A check carrying the ask is already on its way.
    Waiting(i64),
    /// Request a check that carries the ask.
    Request,
}

fn step_of(
    s: &Subject,
    head: &str,
    row: Option<&Row>,
    (project, iid): (ProjectId, i64),
) -> Result<Step, ForgeError> {
    let Some(row) = row else {
        return Ok(Step::Request);
    };
    let same_head = row.checked_head.as_deref() == Some(head);
    match row.status.as_str() {
        "cannot_be_merged" if same_head => Err(conflict("conflict", project, iid)),
        "can_be_merged" if same_head => match (&row.result, &row.checked_target) {
            (Some(result), Some(target)) => Ok(Step::Enqueue {
                result: oid(s.mr, "result_sha", result)?,
                target: oid(s.mr, "checked_target_sha", target)?,
            }),
            // Carried to the tip (plan 18 S4), or not written yet.
            _ => Ok(Step::Request),
        },
        "unchecked" | "checking" if row.asked => Ok(Step::Waiting(row.check_seq)),
        _ => Ok(Step::Request),
    }
}

fn record(change: PipelineChange, project: ProjectId) -> ChangeRecord {
    let details = AuditDetails::Pipeline(change);
    ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

/// A person's ask for the merge request pipeline of merge request `iid`
/// of `project` on the current target (module docs), decided again on the
/// locked project and `caller`. Errors (nothing written): `decide`'s,
/// `FG-REPO-0005`, `FG-CI-0003`, `FG-DB-*`.
pub async fn run_merge_request_pipeline_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(MergePipelineRun, P), ForgeError> {
    let origin = ctx.request_id.clone(); // clone: the jobs own their origin
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let subject = tx.step(|c| subject_on(c, project, iid)).await?;
        let Some(subject) = subject else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "merge_request")
                .with_context("reason", "absent")
                .with_context("project_id", project.get())
                .with_context("iid", iid));
        };
        if !subject.open {
            return Err(conflict("not_open", project, iid));
        }
        let Some(head_raw) = subject.head.clone() else {
            return Err(conflict("no_version", project, iid));
        };
        let head = oid(subject.mr, "head_sha", &head_raw)?;
        let mr = subject.mr;
        let row = tx.step(|c| row_on(c, mr)).await?;
        let run = |outcome, merge_result, target, check_seq| MergePipelineRun {
            outcome,
            merge_request: mr,
            iid,
            merge_result,
            target,
            check_seq,
        };
        match step_of(&subject, &head_raw, row.as_ref(), (project, iid))? {
            Step::Enqueue { result, target } => {
                // Review (plan 18 S5): the pipeline itself, not only its
                // job (a dead job frees its dedup key; a purged one is
                // gone): a row for (merge request, merge result) is
                // `exists`, and nothing is written.
                let source = subject.source.clone(); // clone: moved into the step
                let on = result.clone(); // clone: as above
                let exists = tx
                    .step(|c| async move { pipeline_exists(c, (project, mr), &source, &on).await })
                    .await?;
                if exists {
                    let answer = run(
                        MergePipelineOutcome::Exists,
                        Some(result),
                        Some(target),
                        None,
                    );
                    return Ok(Settle::Discard((answer, decided)));
                }
                let source = subject.source.clone(); // clone: moved into the step
                let (on, origin) = (result.clone(), origin.clone()); // clone: as above
                let head_for = head.clone(); // clone: as above
                let enqueued = tx
                    .step(|c| async move {
                        enqueue_pipeline(c, (project, mr), &source, &head_for, &on, &origin).await
                    })
                    .await?;
                if !enqueued {
                    let answer = run(
                        MergePipelineOutcome::Exists,
                        Some(result),
                        Some(target),
                        None,
                    );
                    return Ok(Settle::Discard((answer, decided)));
                }
                let change = PipelineChange::MergeRequestRun {
                    project,
                    merge_request: mr.get(),
                    iid,
                    outcome: MergePipelineOutcome::Queued.as_str(),
                    sha: Some(result.as_str().to_owned()),
                    target_sha: Some(target.as_str().to_owned()),
                    check_seq: None,
                };
                let answer = run(
                    MergePipelineOutcome::Queued,
                    Some(result),
                    Some(target),
                    None,
                );
                Ok(Settle::Commit(record(change, project), (answer, decided)))
            }
            Step::Waiting(seq) => {
                let answer = run(MergePipelineOutcome::Computing, None, None, Some(seq));
                Ok(Settle::Discard((answer, decided)))
            }
            Step::Request => {
                let origin = origin.clone(); // clone: moved into the step
                let seq = tx
                    .step(|c| async move {
                        request_check_asking_on(c, mr, project, (origin, None), true).await
                    })
                    .await?;
                let change = PipelineChange::MergeRequestRun {
                    project,
                    merge_request: mr.get(),
                    iid,
                    outcome: MergePipelineOutcome::Computing.as_str(),
                    sha: None,
                    target_sha: None,
                    check_seq: Some(seq),
                };
                let answer = run(MergePipelineOutcome::Computing, None, None, Some(seq));
                Ok(Settle::Commit(record(change, project), (answer, decided)))
            }
        }
    }
    .await;
    tx.settle("pipeline.run_merge_request", outcome).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use forge_core::ids::MergeRequestId;
    use forge_core::RequestId;

    use crate::review::mergeability::job_of;
    use crate::review::{mergeability_job, mergeability_payload, pipeline_asked};

    /// A person's ask rides on the mergeability job (`"pipeline": true`,
    /// the same dedup key); a job without it is no ask, and anything but a
    /// boolean is refused.
    #[test]
    fn a_pipeline_ask_rides_on_the_job_and_garbage_is_refused() {
        let id = || RequestId::parse("req-x").expect("id");
        let plain = mergeability_job(MergeRequestId::new(5), 3, id(), None).expect("job");
        assert!(!pipeline_asked(&plain.payload).expect("no ask"));
        let asked = job_of(MergeRequestId::new(5), 3, (id(), None), true).expect("job");
        assert!(pipeline_asked(&asked.payload).expect("ask"));
        assert_eq!(asked.dedup_key, plain.dedup_key);
        assert_eq!(
            mergeability_payload(&asked.payload).expect("payload"),
            mergeability_payload(&plain.payload).expect("payload")
        );
        let bad = serde_json::json!({"merge_request_id": 5, "check_seq": 3, "pipeline": "yes"});
        assert!(pipeline_asked(&bad).is_err());
    }
}

#[cfg(test)]
#[path = "merge_pipeline_tests.rs"]
mod db_tests;
