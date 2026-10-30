//! Project commands (step D4, ADR-0004, ADR-0010, ADR-0015 Amendment D4,
//! ADR-0016): create a project with its namespace, its one repository and
//! its first Owner, and read what `authorize()` needs about a project
//! ([`access`]). Each command is one `ChangeTx` with one `project` audit row
//! (`forge.audited_change_present` refuses anything else).
//!
//! A project lives under a personal namespace or a group, never under
//! another project (step D plan answer 1). Its path is a `namespaces` row
//! with `kind = 'project'` and `owner_id = projects.id` (there is no
//! `projects.namespace_id`, ADR-0015 Amendment D4 MR1); the project, that
//! namespace and the repository are written in ONE transaction, which is
//! what keeps the 1:1:1 (the database does not).

use forge_core::ids::{NamespaceId, ProjectId, RepoId, UserId};
use forge_core::namespace_path::{MAX_DEPTH, MAX_FULL_PATH_LEN};
use forge_core::{AuditDetails, PathSegment, RepositoryKind, Visibility};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::change::{
    Actor, AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome,
};
use crate::jobs::{Enqueued, JobId};
use crate::namespaces::{is_tombstoned, lock_path, path_taken};
use crate::{query_error, read_query_error, repositories, Db};

/// The audit resource type of projects, their repository and members
/// (tables.toml), and the aggregate of their events.
const RESOURCE: &str = "project";

/// The request to create a project. `segment` is the last path segment; the
/// full path is the parent's path plus it.
#[derive(Debug, Clone)]
pub struct NewProject {
    pub segment: String,
    /// The namespace the project goes under: a personal (`user`) or a
    /// `group` namespace.
    pub parent: NamespaceId,
    pub display_name: String,
    pub description: String,
    pub visibility: Visibility,
    /// The kind of the project's repository, fixed at creation (ADR-0004).
    pub kind: RepositoryKind,
    /// The user creating the project, who becomes its direct Owner: an
    /// active human or service account. It must be the acting user of the
    /// change (`Actor::User` / `ServiceAccount` itself, the person of
    /// `UserViaServiceAccount`), else `FG-REPO-0014 creator_not_actor`;
    /// only `Actor::System` (the operator CLI and the worker, the trust root
    /// as for the token commands, ADR-0019 §8) may name another user. A
    /// route must pass its authenticated principal here and decide with
    /// `authorize()` beforehand whether that principal may create under
    /// `parent`; this command does not authorize.
    pub creator: UserId,
}

impl NewProject {
    /// A request with the defaults of step D plan answer 5: an empty
    /// description, visibility `private`, repository kind `system`.
    pub fn new(segment: &str, parent: NamespaceId, display_name: &str, creator: UserId) -> Self {
        Self {
            segment: segment.to_owned(),
            parent,
            display_name: display_name.to_owned(),
            description: String::new(),
            visibility: Visibility::Private,
            kind: RepositoryKind::System,
            creator,
        }
    }
}

/// What `create` wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatedProject {
    pub project_id: ProjectId,
    pub namespace_id: NamespaceId,
    /// In state `provisioning` until the provisioning job marks it ready.
    pub repository_id: RepoId,
    /// The `repository.provision` job (`repositories::PROVISION_JOB`).
    pub provision_job: JobId,
}

/// What `authorize()`'s `ProjectView` needs from the project row besides
/// the caller's role (`roles::load_project_roles`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectAccess {
    pub visibility: Visibility,
    /// Deleted and not yet purged (`deleted_at` set; D5 writes it, and the
    /// purge removes the row after `purge_after`).
    pub pending_deletion: bool,
}

/// `FG-REPO-0014` with its `reason`.
fn request_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ProjectRequestInvalid).with_context("reason", reason)
}

fn not_found(resource: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", resource)
        .with_context("reason", "absent")
}

/// The field rules of forge.projects (its CHECKs, migration
/// 20261031100000), checked before anything is written: `FG-REPO-0014`
/// with `reason` / `len`.
fn validate(new: &NewProject) -> Result<(), ForgeError> {
    let fields = [
        (new.display_name.as_str(), 1, 255, "display_name_length"),
        (new.description.as_str(), 0, 2000, "description_length"),
    ];
    for (value, min, max, reason) in fields {
        let len = value.chars().count();
        if len < min || len > max {
            return Err(request_invalid(reason).with_context("len", len));
        }
        // PostgreSQL text cannot hold NUL; refuse it here, not as a query error.
        if value.contains('\0') {
            return Err(request_invalid("nul_character").with_context("len", len));
        }
    }
    Ok(())
}

/// The creator: present (`FG-REPO-0005 resource=user`), not a bot
/// (`FG-REPO-0010 bot_member`, as for a group's first Owner) and active
/// (`FG-REPO-0014 creator_inactive`). The row is read `FOR SHARE`, so it
/// cannot be disabled before the change commits.
async fn check_creator(conn: &mut PgConnection, user: UserId) -> Result<(), ForgeError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR SHARE")
            .bind(user.get())
            .fetch_optional(conn)
            .await
            .map_err(|e| query_error("projects.create.creator", e))?;
    let Some((kind, state)) = row else {
        return Err(not_found("user"));
    };
    if kind == "bot" {
        return Err(
            ForgeError::new(ErrorCode::MembershipConflict).with_context("reason", "bot_member")
        );
    }
    if state != "active" {
        return Err(request_invalid("creator_inactive"));
    }
    Ok(())
}

/// A unique violation on the path is a taken path; anything else stays a
/// query error.
fn namespace_conflict(e: sqlx::Error) -> ForgeError {
    match e.as_database_error().and_then(|d| d.constraint()) {
        Some("namespaces_sibling_key" | "namespaces_full_path_key") => path_taken("sibling"),
        _ => query_error("projects.create.namespace", e),
    }
}

/// The owner of a personal parent namespace inherits Owner of the project
/// (`roles::load_project_roles`), so it must be an active non-bot account:
/// a bot is `FG-REPO-0010 bot_member` (bots are never members, ADR-0017),
/// an inactive user `FG-REPO-0014 parent_owner_inactive`. The row is read
/// `FOR SHARE`, so it cannot be disabled before the change commits. A
/// personal namespace without its user is corrupt data (`FG-SYS-0001`).
async fn check_namespace_owner(conn: &mut PgConnection, owner_id: i64) -> Result<(), ForgeError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR SHARE")
            .bind(owner_id)
            .fetch_optional(conn)
            .await
            .map_err(|e| query_error("projects.create.parent_owner", e))?;
    let Some((kind, state)) = row else {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "namespace_owner_missing")
            .with_context("user_id", owner_id));
    };
    if kind == "bot" {
        return Err(
            ForgeError::new(ErrorCode::MembershipConflict).with_context("reason", "bot_member")
        );
    }
    if state != "active" {
        return Err(request_invalid("parent_owner_inactive"));
    }
    Ok(())
}

/// `FG-REPO-0014 creator_not_actor` unless `creator` is the acting user;
/// `Actor::System` (operator CLI, worker) may name any creator.
fn check_actor(ctx: &ChangeContext, creator: UserId) -> Result<(), ForgeError> {
    let acting = match &ctx.actor {
        Actor::System => return Ok(()),
        Actor::User(u) | Actor::ServiceAccount(u) => Some(*u),
        Actor::UserViaServiceAccount { user, .. } => Some(*user),
        Actor::Anonymous => None,
    };
    if acting == Some(creator) {
        Ok(())
    } else {
        Err(request_invalid("creator_not_actor"))
    }
}

/// Where the project's namespace goes.
struct Placement {
    full_path: String,
    depth: i16,
}

/// The parent must be a user or group namespace (`FG-REPO-0014
/// parent_not_owner_namespace` for a project), read `FOR SHARE` so it
/// cannot move before the commit; the result must stay within `MAX_DEPTH`
/// and `MAX_FULL_PATH_LEN` (`FG-REPO-0007` `too_deep` / `path_too_long`).
async fn place(
    conn: &mut PgConnection,
    segment: &PathSegment,
    parent: NamespaceId,
) -> Result<Placement, ForgeError> {
    let row: Option<(String, i64, String, i16)> = sqlx::query_as(
        "SELECT kind, owner_id, full_path, depth FROM forge.namespaces WHERE id = $1 FOR SHARE",
    )
    .bind(parent.get())
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("projects.create.parent", e))?;
    let Some((kind, owner_id, parent_path, parent_depth)) = row else {
        return Err(not_found("namespace"));
    };
    match kind.as_str() {
        "group" => {}
        "user" => check_namespace_owner(conn, owner_id).await?,
        _ => return Err(request_invalid("parent_not_owner_namespace")),
    }
    let depth = parent_depth.saturating_add(1);
    if usize::try_from(depth).map_or(true, |d| d > MAX_DEPTH) {
        return Err(ForgeError::new(ErrorCode::NamespacePathInvalid)
            .with_context("reason", "too_deep")
            .with_context("len", depth));
    }
    let full_path = format!("{parent_path}/{}", segment.as_str());
    let len = full_path.chars().count();
    if len > MAX_FULL_PATH_LEN {
        return Err(ForgeError::new(ErrorCode::NamespacePathInvalid)
            .with_context("reason", "path_too_long")
            .with_context("len", len));
    }
    Ok(Placement { full_path, depth })
}

/// Insert the project, its namespace (`kind = 'project'`, `owner_id =
/// projects.id`), its repository (`provisioning`, `new.kind`) and the
/// creator as direct Owner, enqueue the `repository.provision` job
/// (dedup `repository_provision:<repo_id>`, the request id as origin), and
/// record one `project.created` audit row (resource `project`/`projects.id`,
/// `project_id` set) with one `project.created` event of that project — all
/// in one transaction. The path is taken under its advisory lock
/// (`namespaces::lock_path`) and checked against tombstones before and
/// after the insert, as `groups::create` does.
///
/// Errors (nothing is written): `FG-REPO-0007` invalid name (also
/// `too_deep`, `path_too_long`), `FG-REPO-0009` taken / tombstoned,
/// `FG-REPO-0014` a field, a parent that is a project, a personal parent
/// whose owner is inactive, an inactive creator or a creator who is not the
/// acting user, `FG-REPO-0010` `bot_member` (a bot creator or a bot's
/// personal namespace), `FG-REPO-0005` unknown parent
/// namespace or creator (`reason=absent`).
pub async fn create(
    db: &Db,
    ctx: &ChangeContext,
    new: &NewProject,
) -> Result<CreatedProject, ForgeError> {
    // Never top level, so the reserved top-level words are allowed.
    let segment = PathSegment::parse(&new.segment)?;
    validate(new)?;
    check_actor(ctx, new.creator)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let placement = tx.step(|c| place(c, &segment, new.parent)).await?;
    tx.step(|c| check_creator(c, new.creator)).await?;
    let path = placement.full_path.as_str();
    tx.step(|c| lock_path(c, path)).await?;
    if tx.step(|c| is_tombstoned(c, path)).await? {
        return Err(path_taken("tombstoned"));
    }
    let visibility = new.visibility;
    let project_id: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "INSERT INTO forge.projects (display_name, description, visibility) \
                 VALUES ($1, $2, $3) RETURNING id",
            )
            .bind(new.display_name.as_str())
            .bind(new.description.as_str())
            .bind(visibility.as_str())
            .fetch_one(c)
            .await
            .map_err(|e| query_error("projects.create.project", e))
        })
        .await?;
    let (parent, depth) = (new.parent, placement.depth);
    let namespace_id: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "INSERT INTO forge.namespaces (parent_id, kind, owner_id, segment, full_path, \
                 depth) VALUES ($1, 'project', $2, $3, $4, $5) RETURNING id",
            )
            .bind(parent.get())
            .bind(project_id)
            .bind(segment.as_str())
            .bind(path)
            .bind(depth)
            .fetch_one(c)
            .await
            .map_err(namespace_conflict)
        })
        .await?;
    // Defence in depth, as in groups::create: the lock rule is the guarantee.
    if tx.step(|c| is_tombstoned(c, path)).await? {
        return Err(path_taken("tombstoned"));
    }
    let kind = new.kind;
    let repository_id: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "INSERT INTO forge.repositories (project_id, kind) VALUES ($1, $2) RETURNING id",
            )
            .bind(project_id)
            .bind(kind.as_str())
            .fetch_one(c)
            .await
            .map_err(|e| query_error("projects.create.repository", e))
        })
        .await?;
    let owner = new.creator;
    tx.step(|c| async move {
        sqlx::query(
            "INSERT INTO forge.project_members (project_id, user_id, role) \
             VALUES ($1, $2, 'owner')",
        )
        .bind(project_id)
        .bind(owner.get())
        .execute(c)
        .await
        .map_err(|e| query_error("projects.create.owner", e))
    })
    .await?;
    let repository_id = RepoId::new(repository_id);
    let job = repositories::provision_job(repository_id, &ctx.request_id)?;
    let provision_job = match tx.enqueue_job(&job).await? {
        Enqueued::Queued(id) => id,
        // The repository id is new in this transaction: a held key means
        // the queue or the id sequence is wrong, never a retry to absorb.
        Enqueued::Deduped => {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "unexpected_job_dedup")
                .with_context("repo_id", repository_id.get()))
        }
    };
    let created = CreatedProject {
        project_id: ProjectId::new(project_id),
        namespace_id: NamespaceId::new(namespace_id),
        repository_id,
        provision_job,
    };
    let details = AuditDetails::ProjectCreated {
        project_id: created.project_id,
        namespace_id: created.namespace_id,
        parent_namespace_id: new.parent,
        repository_id,
        kind,
        visibility,
        owner,
    };
    tx.commit(created_record(&details, created.project_id)?)
        .await?
        .require_recorded(details.action())?;
    Ok(created)
}

/// The audit row and the event of `project.created`: both on the project
/// (the event scope rule, ADR-0016 Amendment D4); the event's payload is
/// the audit details (ids, kind, visibility). No dedup key: the project id
/// is new, so the change cannot repeat.
fn created_record(details: &AuditDetails, project: ProjectId) -> Result<ChangeRecord, ForgeError> {
    let event_type = details.event_type().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "audit_only_event")
            .with_context("action", details.action())
    })?;
    let value = details.to_value();
    Ok(ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: value.clone(), // clone: the same JSON is the event payload
        },
        events: vec![DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: RESOURCE,
            aggregate_id: project.get(),
            project_id: Some(project),
            payload: value,
            dedup_key: None,
        }],
    })
}

/// The visibility and deletion state of `project`, read on the read pool.
/// `FG-REPO-0005` (`resource=project`, `reason=absent`) when there is no
/// such project: callers answer an absent and an invisible project with
/// the same 404 (ADR-0010 Amendment D2).
pub async fn access(db: &Db, project: ProjectId) -> Result<ProjectAccess, ForgeError> {
    let row: Option<(String, bool)> = db
        .within("projects.access", async {
            sqlx::query_as(
                "SELECT visibility, deleted_at IS NOT NULL FROM forge.projects WHERE id = $1",
            )
            .bind(project.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error("projects.access", e))
        })
        .await?;
    let Some((raw, pending_deletion)) = row else {
        return Err(not_found("project"));
    };
    let Some(visibility) = Visibility::parse(&raw) else {
        // Only if the CHECK projects_visibility is gone: never guess.
        let raw: String = raw.chars().take(32).collect();
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unknown_visibility")
            .with_context("project_id", project.get())
            .with_context("raw", raw));
    };
    Ok(ProjectAccess {
        visibility,
        pending_deletion,
    })
}

#[cfg(test)]
mod tests;
