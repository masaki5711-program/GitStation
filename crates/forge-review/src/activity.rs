//! The recent events of the projects the caller is involved in (plan 15
//! MR 8, §3.4): `GET /api/v1/user/activity?before=&limit=`
//! (`ActivityRead` on the caller's own activity; `authorize()` allows
//! nobody else, administrators included, and people only).
//!
//! "Involved" (plan 15 §6, default 3) is a membership: directly, through an
//! ancestor group, or as the owner of the personal namespace. forge-db
//! reads those projects and their access rows in one statement each; every
//! project is decided here, for the caller's own principal, by the one
//! decision point — this module has no access rule of its own:
//! `ActivityRead` on the project's integration-class events as loaded for
//! the caller now (`forge_auth::prove`). Only the allowed projects (at
//! most [`MAX_INVOLVED_PROJECTS`], newest first; more is said as
//! `truncated`) reach the statement reading the events of the types in
//! [`SHOWN_TYPES`], and each row is decided again for its type's
//! [`VisibilityClass`]. `authorize()` allows the integration class only,
//! Reporter and above by role (administrators with administrator rights
//! pass the role, so they see every project they hold a membership in); a
//! project the caller lost, cannot see or that waits for deletion shows
//! nothing, and its name never appears. A refusal hides; any other
//! decision error is returned (logged once by the route).
//!
//! **Allowlists, never the payload.** The shown types are an explicit,
//! exhaustive decision per [`EventType`] ([`fields_of`]), every one of the
//! integration class (tested); an unknown stored type is never shown. Each
//! type carries only the fields it is allowed: the actor's username and
//! display name, the project's id and path, the time, and per type a merge
//! request's `iid` and current title or a push's ref count. A row that is
//! read and then not shown is only counted (`rows_hidden`, logged by the
//! route with the request id), never described.
//!
//! A page may hold fewer rows than asked (hidden rows are skipped);
//! `next_before` is set whenever more candidates may follow. Events past
//! the retention period are gone from the feed, so they are not listed.

use std::collections::HashMap;

use forge_auth::{prove, Action, Authorized, LoadedProject, ProjectEventsCtx, UserActivityCtx};
use forge_core::{EventType, ProjectId, ProjectRecord, VisibilityClass};
use forge_db::lists;
pub use forge_db::review::activity::MAX_INVOLVED_PROJECTS;
use forge_db::review::activity::{self as rows, ActivityRow};
use forge_errors::{ErrorCode, ForgeError};

use crate::{require, Review};

/// The fields one shown type may carry besides the envelope (type, time,
/// actor, project).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fields {
    /// Nothing more.
    Envelope,
    /// The merge request's `iid` and current title.
    MergeRequest,
    /// The number of refs the push changed.
    Push,
}

/// Whether the activity list shows `t`, and with which fields. Exhaustive
/// on purpose: a new event type is not shown until it is decided here.
fn fields_of(t: EventType) -> Option<Fields> {
    match t {
        EventType::ProjectCreated => Some(Fields::Envelope),
        EventType::RepositoryPushed => Some(Fields::Push),
        EventType::MergeRequestOpened
        | EventType::MergeRequestClosed
        | EventType::MergeRequestReopened
        | EventType::MergeRequestMerged
        | EventType::MergeRequestApproved
        | EventType::MergeRequestCommented => Some(Fields::MergeRequest),
        EventType::RepositoryCreated
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
        | EventType::MergeRequestUpdated
        | EventType::MergeRequestDraftChanged
        | EventType::MergeRequestPushed
        | EventType::MergeRequestConflictDetected
        | EventType::MergeRequestConflictResolved
        | EventType::MergeRequestConflictPredicted
        | EventType::MergeRequestSourceBranchDeleted
        | EventType::MergeRequestUnapproved
        | EventType::CodeownersEnforcementChanged
        | EventType::CodeownersEnforcementRestored
        | EventType::CheckCompleted
        | EventType::WorkItemLinked
        | EventType::WorkItemUnlinked
        | EventType::MergeRequestWorkItemsChanged
        | EventType::MergeRequestRelinkRequested
        | EventType::WebhookDisabled => None,
    }
}

/// The event types the activity list shows, in declaration order.
pub fn shown_types() -> Vec<EventType> {
    EventType::ALL
        .into_iter()
        .filter(|t| fields_of(*t).is_some())
        .collect()
}

/// The number of shown types (`SHOWN_TYPES.len()`).
pub const SHOWN_COUNT: usize = 8;

/// The event types the activity list shows (the `type` values an answer
/// may carry; tested equal to [`shown_types`]).
pub const SHOWN_TYPES: [&str; SHOWN_COUNT] = [
    "project.created",
    "repository.pushed",
    "merge_request.opened",
    "merge_request.closed",
    "merge_request.reopened",
    "merge_request.commented",
    "merge_request.merged",
    "merge_request.approved",
];

/// Who caused an event (a person or an account; `None` on the item for the
/// system or an account that is gone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityActor {
    pub username: String,
    pub display_name: String,
}

/// The merge request an event is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityMergeRequest {
    pub iid: i64,
    /// Its current title (`None`: not found in the event's project).
    pub title: Option<String>,
}

/// One event the caller may see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityItem {
    /// The keyset of `before`.
    pub seq: i64,
    pub event_type: EventType,
    /// UTC, RFC 3339 with milliseconds.
    pub occurred_at: String,
    pub project_id: ProjectId,
    /// The project's path now (`group/project`).
    pub project_path: String,
    pub actor: Option<ActivityActor>,
    /// Merge request events only (`None` when the stored event lacks a
    /// well-formed `iid`).
    pub merge_request: Option<ActivityMergeRequest>,
    /// `repository.pushed` only.
    pub ref_count: Option<i64>,
}

/// One page of the activity list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityPage {
    pub items: Vec<ActivityItem>,
    /// Pass as `before` for the next (older) page; `None`: no more.
    pub next_before: Option<i64>,
    /// The caller is involved (Reporter and above) in more than
    /// [`MAX_INVOLVED_PROJECTS`] projects, or holds more memberships than
    /// one read decides on: the oldest projects were not read.
    pub truncated: bool,
    /// For the route's log only (counts, never what they were): the
    /// projects whose events were read, the membership projects refused,
    /// the candidate rows read and how many of them were not shown.
    pub projects: usize,
    pub projects_hidden: usize,
    pub rows_scanned: usize,
    pub rows_hidden: usize,
}

/// What one page is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityQuery {
    pub before: Option<i64>,
    pub limit: usize,
}

impl Review {
    /// `GET /api/v1/user/activity` (`ActivityRead`): one page of the events
    /// of the projects the caller is involved in, newest first.
    ///
    /// Every membership project is decided once, before any event is read
    /// (`ActivityRead` on its integration-class events): only the allowed
    /// ids reach the event statement, so a Guest-only or hidden membership
    /// costs no scan and takes no page slot, and only allowed projects
    /// count toward [`MAX_INVOLVED_PROJECTS`]. Each row is then decided
    /// again for its own type's class (pure, no I/O: the stored type is
    /// never trusted to be the class decided for the project).
    pub async fn user_activity(
        &self,
        proof: &Authorized<UserActivityCtx>,
        query: ActivityQuery,
    ) -> Result<ActivityPage, ForgeError> {
        require(proof.action(), Action::ActivityRead)?;
        let user = *proof.id();
        let candidates = rows::involved_projects(&self.db, user).await?;
        // One statement for every candidate's access row (review: no N+1).
        let records = lists::project_records(&self.db, &candidates.ids, user).await?;
        let mut allowed: HashMap<ProjectId, ProjectRecord> = HashMap::new();
        let mut ids: Vec<ProjectId> = Vec::new();
        let mut projects_hidden = 0;
        let mut over_cap = false;
        // Newest project first (the records' order).
        for record in records {
            if !allows(proof, &record, VisibilityClass::Integration)? {
                projects_hidden += 1;
                continue;
            }
            if ids.len() == MAX_INVOLVED_PROJECTS {
                over_cap = true;
                continue;
            }
            ids.push(record.id);
            allowed.insert(record.id, record);
        }
        let candidates_rows =
            rows::activity_page(&self.db, &ids, &SHOWN_TYPES, query.before, query.limit).await?;
        let rows_scanned = candidates_rows.len();
        let next_before = (rows_scanned >= query.limit)
            .then(|| candidates_rows.last().map(|r| r.seq))
            .flatten();
        let mut items = Vec::with_capacity(rows_scanned);
        for row in candidates_rows {
            let record = allowed.get(&ProjectId::new(row.project_id));
            if let Some(item) = decide(proof, record, row)? {
                items.push(item);
            }
        }
        Ok(ActivityPage {
            rows_hidden: rows_scanned - items.len(),
            items,
            next_before,
            truncated: candidates.truncated || over_cap,
            projects: ids.len(),
            projects_hidden,
            rows_scanned,
        })
    }
}

/// `authorize()`'s answer for `ActivityRead` on `record`'s events of
/// `class`: `false` on a refusal (not found, forbidden, pending deletion:
/// the row or project is hidden, counted by the caller); any other error
/// (a defect such as a resource of the wrong shape) is returned, and the
/// route logs it once with its `error_code` and the request id.
fn allows(
    proof: &Authorized<UserActivityCtx>,
    record: &ProjectRecord,
    class: VisibilityClass,
) -> Result<bool, ForgeError> {
    let loaded = LoadedProject {
        id: record.id,
        visibility: record.access.visibility,
        role: record.access.role,
        pending_deletion: record.access.pending_deletion,
    };
    match prove(
        proof.principal(),
        Action::ActivityRead,
        ProjectEventsCtx::new(loaded, class),
    ) {
        Ok(_) => Ok(true),
        Err(e)
            if matches!(
                e.code(),
                ErrorCode::NotFound | ErrorCode::Forbidden | ErrorCode::PendingDeletion
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(e.with_context("during", "activity_decision")),
    }
}

/// The row as the caller may see it, `None` when it is hidden (an unknown
/// or unshown type, a project not allowed, a refusal for the type's
/// class); an unexpected decision error is returned.
fn decide(
    proof: &Authorized<UserActivityCtx>,
    record: Option<&ProjectRecord>,
    row: ActivityRow,
) -> Result<Option<ActivityItem>, ForgeError> {
    let Some(event_type) = EventType::parse(&row.event_type) else {
        return Ok(None);
    };
    let Some(fields) = fields_of(event_type) else {
        return Ok(None);
    };
    let Some(record) = record else {
        return Ok(None);
    };
    if !allows(proof, record, event_type.visibility_class())? {
        return Ok(None);
    }
    let actor = match (
        row.actor_kind.as_str(),
        row.actor_username,
        row.actor_display_name,
    ) {
        ("system", _, _) => None,
        (_, Some(username), Some(display_name)) => Some(ActivityActor {
            username,
            display_name,
        }),
        _ => None,
    };
    let (merge_request, ref_count) = match fields {
        Fields::Envelope => (None, None),
        Fields::MergeRequest => (
            row.iid.map(|iid| ActivityMergeRequest {
                iid,
                title: row.title,
            }),
            None,
        ),
        Fields::Push => (None, row.total_refs),
    };
    Ok(Some(ActivityItem {
        seq: row.seq,
        event_type,
        occurred_at: row.occurred_at,
        project_id: record.id,
        project_path: record.path.full_path(),
        actor,
        merge_request,
        ref_count,
    }))
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn every_shown_type_is_of_the_integration_class_and_listed() {
        let shown = shown_types();
        assert_eq!(
            shown.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            SHOWN_TYPES.to_vec()
        );
        for t in shown {
            assert_eq!(
                t.visibility_class(),
                VisibilityClass::Integration,
                "{}",
                t.as_str()
            );
        }
    }

    #[test]
    fn maintainer_and_admin_class_types_are_never_shown() {
        for t in EventType::ALL {
            if t.visibility_class() != VisibilityClass::Integration {
                assert_eq!(fields_of(t), None, "{}", t.as_str());
            }
        }
    }
}
