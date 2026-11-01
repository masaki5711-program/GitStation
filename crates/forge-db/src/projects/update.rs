//! Project updates decided inside their transaction (M1 phase 8b MR7a):
//! the display name / description ([`update_decided`], `project.updated`,
//! audit-only in M1) and the visibility ([`change_visibility_decided`],
//! `project.visibility_changed`, an event of the maintainer class).
//!
//! Each locks the project row `FOR UPDATE` (with what it reads of it),
//! then re-reads the caller's roles under `FOR SHARE` on the membership
//! rows (`roles::lock_roles`), then locks the caller's account row — the
//! lock order of `projects::members` — and hands those facts to `decide`
//! (forge-hosting: `Authorized::reconfirm`) before anything is written. A
//! role removed, a demotion or a disable that commits first is seen; one
//! that comes later waits until this change has committed.

use forge_core::ids::{ProjectId, UserId};
use forge_core::{AuditDetails, ProjectAccess, Visibility};
use forge_errors::{ErrorCode, ForgeError};

use super::{evented_record, not_found, request_invalid, RESOURCE};
use crate::access::{lock_account, Account};
use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::groups::{changed_fields, check_fields, Updated};
use crate::roles::{lock_roles, RoleAnchor};
use crate::{query_error, Db};

/// A change to a project's display name and / or description (`None` =
/// unchanged).
#[derive(Debug, Clone, Default)]
pub struct ProjectPatch {
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// A project as a write transaction holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectAccessFacts {
    pub project: ProjectId,
    /// The visibility, deletion and the caller's native role, re-read
    /// under the locks.
    pub access: ProjectAccess,
}

/// What `decide` sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectUpdateFacts {
    pub account: Account,
    pub project: ProjectAccessFacts,
}

/// What a visibility change did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityOutcome {
    /// Written: one audit row and one `project.visibility_changed` event.
    Changed { from: Visibility },
    /// The project already had that visibility: nothing written.
    Unchanged,
}

/// The project row as locked.
struct Locked {
    visibility: Visibility,
    pending_deletion: bool,
    display_name: String,
    description: String,
}

/// Lock the project row (`FOR UPDATE`). `FG-REPO-0005 resource=project`
/// when it does not exist; a project without its namespace is
/// `FG-SYS-0001 project_without_namespace` (as `members::lock_project`), an
/// unknown stored visibility `FG-DB-0021 unknown_visibility`.
async fn lock(tx: &mut ChangeTx, project: ProjectId) -> Result<Locked, ForgeError> {
    let row: Option<(String, bool, String, String, bool)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT p.visibility, p.deleted_at IS NOT NULL, p.display_name, p.description, \
                 n.id IS NOT NULL FROM forge.projects p \
                 LEFT JOIN forge.namespaces n ON n.kind = 'project' AND n.owner_id = p.id \
                 WHERE p.id = $1 FOR UPDATE OF p",
            )
            .bind(project.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("projects.update.lock", e))
        })
        .await?;
    let Some((visibility, pending_deletion, display_name, description, has_namespace)) = row else {
        return Err(not_found("project"));
    };
    if !has_namespace {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "project_without_namespace")
            .with_context("project_id", project.get()));
    }
    let visibility = Visibility::parse(&visibility).ok_or_else(|| {
        ForgeError::new(ErrorCode::NamespaceDataInvalid)
            .with_context("reason", "unknown_visibility")
            .with_context("project_id", project.get())
    })?;
    Ok(Locked {
        visibility,
        pending_deletion,
        display_name,
        description,
    })
}

/// The facts `decide` sees: the locked row, then the roles (membership
/// rows `FOR SHARE`), then the account row.
async fn facts(
    tx: &mut ChangeTx,
    project: ProjectId,
    viewer: UserId,
    locked: &Locked,
) -> Result<ProjectUpdateFacts, ForgeError> {
    let roles = tx
        .step(|c| lock_roles(c, RoleAnchor::Project(project), viewer))
        .await?;
    let account = tx.step(|c| lock_account(c, viewer)).await?;
    Ok(ProjectUpdateFacts {
        account,
        project: ProjectAccessFacts {
            project,
            access: ProjectAccess {
                visibility: locked.visibility,
                role: roles.native,
                pending_deletion: locked.pending_deletion,
            },
        },
    })
}

/// Change a project's display name and / or description for `viewer`,
/// deciding inside the transaction (module docs). One `project.updated`
/// audit row on the project (audit-only in M1) naming the changed fields,
/// or [`Updated::Unchanged`] (nothing written). Every exit that does not
/// commit rolls back explicitly (`ChangeTx::settle`); the field rules run
/// after `decide`. Errors: `FG-REPO-0014` (a
/// field, `empty_update`), `FG-REPO-0005 resource=project`, and whatever
/// `decide` returns.
pub async fn update_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    patch: &ProjectPatch,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(Updated, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock(&mut tx, project).await?;
        let decided = decide(&facts(&mut tx, project, viewer, &locked).await?)?;
        // The field rules after the decision: a caller who may not update
        // gets the denial, never a hint about the body.
        if patch.display_name.is_none() && patch.description.is_none() {
            return Err(request_invalid("empty_update"));
        }
        check_fields(
            patch.display_name.as_deref(),
            patch.description.as_deref(),
            request_invalid,
        )?;
        let fields = changed_fields(
            patch.display_name.as_deref(),
            patch.description.as_deref(),
            &locked.display_name,
            &locked.description,
        );
        if fields.is_empty() {
            return Ok(Settle::Discard((Updated::Unchanged, decided)));
        }
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.projects SET display_name = coalesce($2, display_name), \
                 description = coalesce($3, description), updated_at = now() WHERE id = $1",
            )
            .bind(project.get())
            .bind(patch.display_name.as_deref())
            .bind(patch.description.as_deref())
            .execute(c)
            .await
            .map_err(|e| query_error("projects.update", e))
        })
        .await?;
        let details = AuditDetails::ProjectUpdated {
            project_id: project,
            fields: fields.clone(), // clone: the caller gets the list too
        };
        let mut record = ChangeRecord::audit_only(&details, RESOURCE, project.get());
        // A project's audit row names its project (the event scope rule).
        record.audit.project_id = Some(project);
        Ok(Settle::Commit(record, (Updated::Changed(fields), decided)))
    }
    .await;
    tx.settle("project.updated", outcome).await
}

/// Set a project's visibility to `to` for `viewer`, deciding inside the
/// transaction (module docs; `decide` sees the current visibility). One
/// `project.visibility_changed` audit row and event (maintainer class)
/// with `from` / `to`, or [`VisibilityOutcome::Unchanged`] when it already
/// is `to` (nothing written). Every exit that does not commit rolls back
/// explicitly (`ChangeTx::settle`). Errors: `FG-REPO-0005
/// resource=project` and whatever `decide` returns.
pub async fn change_visibility_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    to: Visibility,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(VisibilityOutcome, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock(&mut tx, project).await?;
        let decided = decide(&facts(&mut tx, project, viewer, &locked).await?)?;
        let from = locked.visibility;
        if from == to {
            return Ok(Settle::Discard((VisibilityOutcome::Unchanged, decided)));
        }
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.projects SET visibility = $2, updated_at = now() WHERE id = $1",
            )
            .bind(project.get())
            .bind(to.as_str())
            .execute(c)
            .await
            .map_err(|e| query_error("projects.change_visibility", e))
        })
        .await?;
        let details = AuditDetails::ProjectVisibilityChanged {
            project_id: project,
            from,
            to,
        };
        let record = evented_record(&details, project)?;
        Ok(Settle::Commit(
            record,
            (VisibilityOutcome::Changed { from }, decided),
        ))
    }
    .await;
    tx.settle("project.visibility_changed", outcome).await
}
