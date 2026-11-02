//! The merge request commands, each decided inside its own transaction
//! (module docs of `review`): [`open`] (`open_mr_decided`), [`edit`]
//! (`edit_mr_decided`) and [`state`] (`close_mr_decided`,
//! `reopen_mr_decided`). None of them authorizes: forge-review passes
//! `decide` = `Authorized::reconfirm` on the facts read under the locks.
//! Every command returns the row as it stands after the transaction (also
//! when nothing changed), so the route answers without another read.

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{AuditDetails, DedupKey, DedupPart, EventType, MergeRequestChange, RefName};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{Row, StoredMergeRequest, COLUMNS};
use crate::access::Account;
use crate::change::{AuditEntry, ChangeRecord, ChangeTx, DomainEvent, Outcome};
use crate::projects::{lock_access, locked_access_facts, ProjectAccessFacts};
use crate::query_error;

mod edit;
mod open;
mod state;

pub use edit::{edit_mr_decided, MergeRequestPatch};
pub use open::{open_mr_decided, NewMergeRequest};
pub use state::{close_mr_decided, reopen_mr_decided};

/// The audit resource of every merge request change (tables.toml).
const RESOURCE: &str = "project";
/// The events' aggregate.
const AGGREGATE: &str = "merge_request";

/// What `decide` sees: the caller's account, the project and the
/// caller's role on it, and the merge request (`None` for an open, and
/// for an iid the project does not have) — all read under the locks.
#[derive(Debug, Clone)]
pub struct MergeRequestFacts {
    pub account: Account,
    pub project: ProjectAccessFacts,
    pub merge_request: Option<StoredMergeRequest>,
}

/// What an edit did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    /// One audit row and one event per transition (`updated`,
    /// `draft_changed`).
    Changed(Vec<EventType>),
    /// Every asked value was already stored: nothing written.
    Unchanged,
}

/// What a close or a reopen did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateOutcome {
    Changed,
    /// Already in that state: nothing written.
    Unchanged,
}

pub(super) fn absent() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "merge_request")
        .with_context("reason", "absent")
}

/// `FG-REVIEW-0001` (409): who asked, on which project, for which change.
pub(super) fn duplicate(
    reason: &'static str,
    project: ProjectId,
    viewer: UserId,
    action: &'static str,
) -> ForgeError {
    ForgeError::new(ErrorCode::DuplicateOpenMergeRequest)
        .with_context("reason", reason)
        .with_context("project_id", project.get())
        .with_context("user_id", viewer.get())
        .with_context("action", action)
}

/// A write the state machine forbids reached the command although
/// `decide` allowed it: a broken invariant, never a pass
/// (`FG-SYS-0001 reason=edit_not_open|close_merged|reopen_merged`).
pub(super) fn state_invariant(reason: &'static str, mr: &StoredMergeRequest) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("merge_request_id", mr.id.get())
        .with_context("state", mr.state.as_str())
}

/// `FG-WEB-0006 param_invalid` for a value a table CHECK refused: the
/// backstop of forge-review's field rules and authorize()'s branch rules
/// (the request was refused, not the database broken).
fn check_refused(param: &'static str, rule: &'static str, constraint: &str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "param_invalid")
        .with_context("param", param)
        .with_context("rule", rule)
        .with_context("constraint", constraint.to_owned())
}

/// The error of a merge request write: the partial unique index
/// `merge_requests_one_open` is FG-REVIEW-0001 (`unique_index`, the backstop
/// of the check under the lock); a CHECK on a request value is
/// FG-WEB-0006 (`param_invalid`); anything else a query error.
pub(super) fn write_error(
    query: &'static str,
    project: ProjectId,
    viewer: UserId,
    action: &'static str,
) -> impl FnOnce(sqlx::Error) -> ForgeError {
    move |e| {
        let constraint = e
            .as_database_error()
            .and_then(|d| d.constraint())
            .map(str::to_owned);
        match constraint.as_deref() {
            Some("merge_requests_one_open") => duplicate("unique_index", project, viewer, action),
            Some(c @ "merge_requests_branches_differ") => {
                check_refused("target_branch", "source_equals_target", c)
            }
            Some(c @ ("merge_requests_source_branch" | "merge_requests_target_branch")) => {
                let param = if c.contains("source") {
                    "source_branch"
                } else {
                    "target_branch"
                };
                check_refused(param, "not_a_branch", c)
            }
            Some(c @ "merge_requests_title_length") => check_refused("title", "length", c),
            Some(c @ "merge_requests_description_length") => {
                check_refused("description", "too_long", c)
            }
            Some(_) | None => query_error(query, e),
        }
    }
}

/// The open merge request of `project` from `source` into `target` other
/// than `except`, if any (read under the project lock).
async fn open_iid(
    conn: &mut PgConnection,
    project: ProjectId,
    source: &RefName,
    target: &RefName,
    except: Option<MergeRequestId>,
) -> Result<Option<i64>, ForgeError> {
    sqlx::query_scalar(
        "SELECT iid FROM forge.merge_requests WHERE project_id = $1 AND source_branch = $2 \
         AND target_branch = $3 AND state = 'open' AND ($4::bigint IS NULL OR id <> $4) LIMIT 1",
    )
    .bind(project.get())
    .bind(source.as_str())
    .bind(target.as_str())
    .bind(except.map(MergeRequestId::get))
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("review.open_iid", e))
}

/// `FG-REVIEW-0001 open_exists` when another open merge request has the
/// pair (checked under the project lock).
pub(super) async fn refuse_open_duplicate(
    tx: &mut ChangeTx,
    project: ProjectId,
    viewer: UserId,
    (source, target): (&RefName, &RefName),
    except: Option<MergeRequestId>,
    action: &'static str,
) -> Result<(), ForgeError> {
    let (source, target) = (source.clone(), target.clone()); // clone: moved into the step
    if let Some(existing) = tx
        .step(|c| async move { open_iid(c, project, &source, &target, except).await })
        .await?
    {
        return Err(duplicate("open_exists", project, viewer, action)
            .with_context("existing_iid", existing));
    }
    Ok(())
}

/// Lock the project row, merge request `iid` (when `Some`), then the
/// caller's roles and account (module docs of `review`).
pub(super) async fn facts(
    tx: &mut ChangeTx,
    project: ProjectId,
    iid: Option<i64>,
    viewer: UserId,
) -> Result<MergeRequestFacts, ForgeError> {
    let locked = lock_access(tx, project).await?;
    let merge_request = match iid {
        None => None,
        Some(iid) => {
            let row: Option<Row> = tx
                .step(|c| async move {
                    sqlx::query_as(&format!(
                        "SELECT {COLUMNS} FROM forge.merge_requests \
                         WHERE project_id = $1 AND iid = $2 FOR UPDATE"
                    ))
                    .bind(project.get())
                    .bind(iid)
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("review.lock", e))
                })
                .await?;
            row.map(Row::parse).transpose()?
        }
    };
    let caller = locked_access_facts(tx, project, viewer, &locked).await?;
    Ok(MergeRequestFacts {
        account: caller.account,
        project: caller.project,
        merge_request,
    })
}

/// The audit rows and events of `changes` (one each, in order) at
/// `revision`: the first change's row is the change's own, the others its
/// `affected` rows (all on the project).
pub(super) fn record(
    project: ProjectId,
    changes: Vec<MergeRequestChange>,
    revision: i64,
) -> Result<ChangeRecord, ForgeError> {
    let mut rows = Vec::with_capacity(changes.len());
    let mut events = Vec::with_capacity(changes.len());
    for change in changes {
        let merge_request = change.merge_request();
        let prefix = change.dedup_prefix();
        let details = AuditDetails::MergeRequest(change);
        let event_type = details.event_type().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "audit_only_event")
                .with_context("action", details.action())
        })?;
        let value = details.to_value();
        rows.push(AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: value.clone(), // clone: the same JSON is the event payload
        });
        events.push(DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: AGGREGATE,
            aggregate_id: merge_request.get(),
            project_id: Some(project),
            payload: value,
            dedup_key: Some(DedupKey::new(
                prefix,
                &[DedupPart::Id(merge_request.get()), DedupPart::Id(revision)],
            )?),
        });
    }
    let mut rows = rows.into_iter();
    let audit = rows.next().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "empty_merge_request_change")
    })?;
    Ok(ChangeRecord {
        audit,
        affected: rows.collect(),
        events,
    })
}
