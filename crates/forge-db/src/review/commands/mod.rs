//! The merge request commands, each decided inside its own transaction
//! (module docs of `review`). None of them authorizes: forge-review passes
//! `decide` = `Authorized::reconfirm` on the facts read under the locks.

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{
    AuditDetails, DedupKey, DedupPart, EventType, MergeRequestChange, MergeRequestField,
    MergeRequestState, RefName,
};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{Row, StoredMergeRequest, COLUMNS};
use crate::access::Account;
use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome, Settle,
};
use crate::projects::{lock_access, locked_access_facts, ProjectAccessFacts};
use crate::{query_error, Db};

/// The audit resource of every merge request change (tables.toml).
const RESOURCE: &str = "project";
/// The events' aggregate.
const AGGREGATE: &str = "merge_request";

/// A merge request to open; the fields are checked by forge-review (and
/// the table's CHECKs), the branches are `refs/heads/*` (authorize()).
#[derive(Debug, Clone)]
pub struct NewMergeRequest {
    pub title: String,
    pub description: String,
    pub source_branch: RefName,
    pub target_branch: RefName,
    pub draft: bool,
}

/// What an edit asks for (`None` = unchanged).
#[derive(Debug, Clone, Default)]
pub struct MergeRequestPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub draft: Option<bool>,
    pub target_branch: Option<RefName>,
}

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

fn absent() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "merge_request")
        .with_context("reason", "absent")
}

/// `FG-REVIEW-0001` (409).
fn duplicate(reason: &'static str, project: ProjectId, action: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::DuplicateOpenMergeRequest)
        .with_context("reason", reason)
        .with_context("project_id", project.get())
        .with_context("action", action)
}

/// A write the state machine forbids reached the command although
/// `decide` allowed it: a broken invariant, never a pass.
fn state_invariant(reason: &'static str, mr: &StoredMergeRequest) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("merge_request_id", mr.id.get())
        .with_context("state", mr.state.as_str())
}

/// A unique violation on `merge_requests_one_open` is FG-REVIEW-0001
/// (`unique_index`, the backstop of the check under the lock).
fn write_error(
    query: &'static str,
    project: ProjectId,
    action: &'static str,
) -> impl FnOnce(sqlx::Error) -> ForgeError {
    move |e| match e.as_database_error().and_then(|d| d.constraint()) {
        Some("merge_requests_one_open") => duplicate("unique_index", project, action),
        _ => query_error(query, e),
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

async fn refuse_open_duplicate(
    tx: &mut ChangeTx,
    project: ProjectId,
    (source, target): (&RefName, &RefName),
    except: Option<MergeRequestId>,
    action: &'static str,
) -> Result<(), ForgeError> {
    let (source, target) = (source.clone(), target.clone()); // clone: moved into the step
    if let Some(existing) = tx
        .step(|c| async move { open_iid(c, project, &source, &target, except).await })
        .await?
    {
        return Err(
            duplicate("open_exists", project, action).with_context("existing_iid", existing)
        );
    }
    Ok(())
}

/// Lock the project row, merge request `iid` (when `Some`), then the
/// caller's roles and account (module docs).
async fn facts(
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
fn record(
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

/// Open a merge request in `project` for `viewer`, deciding inside the
/// transaction (module docs): `decide` sees `merge_request: None`. After it:
/// `FG-REVIEW-0001` when an open one has the same branches (under the
/// project lock; `unique_index` as the backstop), then the next `iid` of
/// the project (its counter row, never `max(iid) + 1`), the row, one
/// `merge_request.opened` audit row and event. Errors also: `FG-REPO-0005
/// resource=project`, and whatever `decide` returns.
pub async fn open_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    new: &NewMergeRequest,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(StoredMergeRequest, P), ForgeError> {
    const ACTION: &str = "merge_request.opened";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, None, viewer).await?;
        let decided = decide(&facts)?;
        refuse_open_duplicate(
            &mut tx,
            project,
            (&new.source_branch, &new.target_branch),
            None,
            ACTION,
        )
        .await?;
        let iid: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "INSERT INTO forge.merge_request_counters (project_id, last_iid) \
                     VALUES ($1, 1) ON CONFLICT (project_id) DO UPDATE \
                     SET last_iid = forge.merge_request_counters.last_iid + 1 RETURNING last_iid",
                )
                .bind(project.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("review.next_iid", e))
            })
            .await?;
        let row: Row = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "INSERT INTO forge.merge_requests (project_id, iid, title, description, \
                     author_id, source_branch, target_branch, draft) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {COLUMNS}"
                ))
                .bind(project.get())
                .bind(iid)
                .bind(&new.title)
                .bind(&new.description)
                .bind(viewer.get())
                .bind(new.source_branch.as_str())
                .bind(new.target_branch.as_str())
                .bind(new.draft)
                .fetch_one(c)
                .await
                .map_err(write_error("review.open", project, ACTION))
            })
            .await?;
        let stored = row.parse()?;
        let change = MergeRequestChange::Opened {
            merge_request: stored.id,
            iid: stored.iid,
            author: viewer,
            source_branch: stored.source_branch.clone(), // clone: the row is returned too
            target_branch: stored.target_branch.clone(),
            draft: stored.draft,
        };
        let record = record(project, vec![change], stored.revision)?;
        Ok(Settle::Commit(record, (stored, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// The transitions an edit of `current` by `patch` makes.
fn edits(current: &StoredMergeRequest, patch: &MergeRequestPatch) -> Vec<MergeRequestChange> {
    let mut fields = Vec::new();
    if patch.title.as_ref().is_some_and(|t| *t != current.title) {
        fields.push(MergeRequestField::Title);
    }
    if patch
        .description
        .as_ref()
        .is_some_and(|d| *d != current.description)
    {
        fields.push(MergeRequestField::Description);
    }
    let retarget = patch
        .target_branch
        .as_ref()
        .filter(|t| **t != current.target_branch);
    if retarget.is_some() {
        fields.push(MergeRequestField::TargetBranch);
    }
    let mut changes = Vec::new();
    if !fields.is_empty() {
        changes.push(MergeRequestChange::Updated {
            merge_request: current.id,
            iid: current.iid,
            fields,
            target_branch: retarget.map(|to| (current.target_branch.clone(), to.clone())), // clone: the details own the names
        });
    }
    if let Some(draft) = patch.draft.filter(|d| *d != current.draft) {
        changes.push(MergeRequestChange::DraftChanged {
            merge_request: current.id,
            iid: current.iid,
            draft,
        });
    }
    changes
}

/// Edit merge request `iid` of `project`: the title, description, draft
/// flag and target asked in `patch`, decided inside the transaction on the
/// row as locked. After `decide`: `FG-REPO-0005 resource=merge_request`
/// for an absent one, `FG-REVIEW-0001` when a new target collides with an
/// open one. One audit row and event per transition (`updated`,
/// `draft_changed`), or [`EditOutcome::Unchanged`] (nothing written).
pub async fn edit_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    viewer: UserId,
    patch: &MergeRequestPatch,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(EditOutcome, P), ForgeError> {
    const ACTION: &str = "merge_request.updated";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.merge_request.ok_or_else(absent)?;
        let changes = edits(&current, patch);
        if changes.is_empty() {
            return Ok(Settle::Discard((EditOutcome::Unchanged, decided)));
        }
        let retargets = changes.iter().any(|c| {
            matches!(
                c,
                MergeRequestChange::Updated {
                    target_branch: Some(_),
                    ..
                }
            )
        });
        if (retargets || patch.draft.is_some_and(|d| d != current.draft))
            && current.state != MergeRequestState::Open
        {
            return Err(state_invariant("edit_not_open", &current));
        }
        let target = patch
            .target_branch
            .clone() // clone: bound into the statement below
            .unwrap_or_else(|| current.target_branch.clone());
        if retargets {
            refuse_open_duplicate(
                &mut tx,
                project,
                (&current.source_branch, &target),
                Some(current.id),
                ACTION,
            )
            .await?;
        }
        let title = patch.title.clone().unwrap_or_else(|| current.title.clone()); // clone: bound below
        let description = patch
            .description
            .clone()
            .unwrap_or_else(|| current.description.clone());
        let draft = patch.draft.unwrap_or(current.draft);
        let id = current.id;
        let revision: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "UPDATE forge.merge_requests SET title = $2, description = $3, draft = $4, \
                     target_branch = $5, revision = revision + 1, updated_at = now() \
                     WHERE id = $1 RETURNING revision",
                )
                .bind(id.get())
                .bind(&title)
                .bind(&description)
                .bind(draft)
                .bind(target.as_str())
                .fetch_one(c)
                .await
                .map_err(write_error("review.edit", project, ACTION))
            })
            .await?;
        let types = changes
            .iter()
            .map(|c| c.transition().event_type())
            .collect();
        let record = record(project, changes, revision)?;
        Ok(Settle::Commit(
            record,
            (EditOutcome::Changed(types), decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Close merge request `iid` of `project` (`closed_at`, `closed_by` =
/// `viewer`), decided inside the transaction. After `decide`:
/// `FG-REPO-0005 resource=merge_request` for an absent one;
/// [`StateOutcome::Unchanged`] for a closed one; a merged one is a broken
/// invariant (`authorize()` denies it, `FG-SYS-0001 close_merged`).
pub async fn close_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    viewer: UserId,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(StateOutcome, P), ForgeError> {
    const ACTION: &str = "merge_request.closed";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.merge_request.ok_or_else(absent)?;
        match current.state {
            MergeRequestState::Closed => {
                return Ok(Settle::Discard((StateOutcome::Unchanged, decided)))
            }
            MergeRequestState::Merged => return Err(state_invariant("close_merged", &current)),
            MergeRequestState::Open => {}
        }
        let id = current.id;
        let revision: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "UPDATE forge.merge_requests SET state = 'closed', closed_at = now(), \
                     closed_by = $2, revision = revision + 1, updated_at = now() \
                     WHERE id = $1 RETURNING revision",
                )
                .bind(id.get())
                .bind(viewer.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("review.close", e))
            })
            .await?;
        let change = MergeRequestChange::Closed {
            merge_request: id,
            iid: current.iid,
        };
        let record = record(project, vec![change], revision)?;
        Ok(Settle::Commit(record, (StateOutcome::Changed, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Reopen merge request `iid` of `project`, decided inside the
/// transaction. After `decide`: `FG-REPO-0005 resource=merge_request` for
/// an absent one; [`StateOutcome::Unchanged`] for an open one; a merged one
/// is a broken invariant (`FG-SYS-0001 reopen_merged`); `FG-REVIEW-0001`
/// when another open one has its branches.
pub async fn reopen_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    viewer: UserId,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(StateOutcome, P), ForgeError> {
    const ACTION: &str = "merge_request.reopened";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.merge_request.ok_or_else(absent)?;
        match current.state {
            MergeRequestState::Open => {
                return Ok(Settle::Discard((StateOutcome::Unchanged, decided)))
            }
            MergeRequestState::Merged => return Err(state_invariant("reopen_merged", &current)),
            MergeRequestState::Closed => {}
        }
        refuse_open_duplicate(
            &mut tx,
            project,
            (&current.source_branch, &current.target_branch),
            Some(current.id),
            ACTION,
        )
        .await?;
        let id = current.id;
        let revision: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "UPDATE forge.merge_requests SET state = 'open', closed_at = NULL, \
                     closed_by = NULL, revision = revision + 1, updated_at = now() \
                     WHERE id = $1 RETURNING revision",
                )
                .bind(id.get())
                .fetch_one(c)
                .await
                .map_err(write_error("review.reopen", project, ACTION))
            })
            .await?;
        let change = MergeRequestChange::Reopened {
            merge_request: id,
            iid: current.iid,
        };
        let record = record(project, vec![change], revision)?;
        Ok(Settle::Commit(record, (StateOutcome::Changed, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
