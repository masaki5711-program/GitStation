//! Project commands (step D4, ADR-0004, ADR-0010, ADR-0015 Amendment D4,
//! ADR-0016): create a project with its namespace, its one repository and
//! its first Owner, change its direct members ([`add_member`],
//! [`change_member`], [`remove_member`], D4 MR4). What `authorize()` needs
//! about a project is read by `crate::resolve::project_access` (one
//! statement with the viewer's role, M1 phase 8 MR3). Each command is one `ChangeTx` with one `project` audit row
//! (`forge.audited_change_present` refuses anything else).
//!
//! A project lives under a personal namespace or a group, never under
//! another project (step D plan answer 1). Its path is a `namespaces` row
//! with `kind = 'project'` and `owner_id = projects.id` (there is no
//! `projects.namespace_id`, ADR-0015 Amendment D4 MR1); the project, that
//! namespace and the repository are written in ONE transaction, which is
//! what keeps the 1:1:1 (the database does not).

use forge_core::ids::{GroupId, NamespaceId, ProjectId, RepoId, UserId};
use forge_core::namespace_path::{MAX_DEPTH, MAX_FULL_PATH_LEN};
use forge_core::{AuditDetails, PathSegment, RepositoryKind, Role, Visibility};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::access::{lock_account, Account};
use crate::change::{
    Actor, AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome, Settle,
};
use crate::jobs::{Enqueued, JobId};
use crate::namespaces::{is_tombstoned, lock_path, path_taken};
use crate::roles::{lock_roles, RoleAnchor};
use crate::{query_error, repositories, Db};

mod member_writes;
mod members;
pub(crate) mod ownership;
mod update;

pub use crate::groups::MemberChange;
pub use crate::groups::Updated;
pub use member_writes::{
    add_member_decided, change_member_decided, remove_member_decided, ProjectMemberFacts,
};
pub(crate) use members::lock_project;
pub use members::{add_member, change_member, remove_member};
pub use ownership::{ownerless, Ownerless};
pub use update::{
    change_visibility_decided, update_decided, ProjectAccessFacts, ProjectPatch,
    ProjectUpdateFacts, VisibilityOutcome,
};
pub(crate) use update::{facts as locked_access_facts, lock as lock_access};

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

/// The parent namespace: a group's id, or a personal namespace's user.
#[derive(Debug, Clone, Copy)]
enum ParentOwner {
    Group(GroupId),
    User(UserId),
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

/// **Callers**: the operator CLI and the worker (actor `system`) and the
/// tests. A route never calls this directly: it goes through forge-hosting,
/// whose `create_project` uses [`create_decided`] (the decision inside the
/// transaction). Only forge-hosting calls the `*_decided` commands
/// (`forge-hosting/tests/decided_callers.rs`).
///
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
    type NoDecision = fn(&ProjectCreateFacts) -> Result<(), ForgeError>;
    let (created, _) = create_in::<NoDecision, ()>(db, ctx, new, None).await?;
    Ok(created)
}

/// [`create`] with the authorization decided **inside** the transaction
/// (M1 phase 8b MR7a): after the parent namespace is locked (`place`,
/// `FOR SHARE`), the creator's role on a group parent is re-read under
/// `FOR SHARE` on the membership rows and the creator's account row is
/// locked; `decide` (forge-hosting: `Authorized::reconfirm`) sees those
/// facts before anything is written, and its error rolls back. Errors as
/// [`create`], plus whatever `decide` returns.
pub async fn create_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    new: &NewProject,
    decide: impl FnOnce(&ProjectCreateFacts) -> Result<P, ForgeError>,
) -> Result<(CreatedProject, P), ForgeError> {
    let (created, decided) = create_in(db, ctx, new, Some(decide)).await?;
    let decided = decided.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "decision_missing")
    })?;
    Ok((created, decided))
}

/// The parent of a project as the create transaction holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentAccess {
    /// A group namespace and the creator's native role on the group.
    Group {
        namespace: NamespaceId,
        group: GroupId,
        role: Option<Role>,
    },
    /// A personal namespace and its owner.
    User {
        namespace: NamespaceId,
        owner: UserId,
    },
}

/// What [`create_decided`]'s `decide` sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectCreateFacts {
    pub account: Account,
    pub parent: ParentAccess,
}

async fn create_in<D, P>(
    db: &Db,
    ctx: &ChangeContext,
    new: &NewProject,
    decide: Option<D>,
) -> Result<(CreatedProject, Option<P>), ForgeError>
where
    D: FnOnce(&ProjectCreateFacts) -> Result<P, ForgeError>,
{
    check_actor(ctx, new.creator)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = create_steps(&mut tx, ctx, new, decide).await;
    tx.settle("project.created", outcome).await
}

/// The parent's kind and owner, locked `FOR SHARE` (as `place` reads it):
/// what `decide` needs before any name or field rule is checked.
async fn lock_parent_owner(
    conn: &mut PgConnection,
    parent: NamespaceId,
) -> Result<ParentOwner, ForgeError> {
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT kind, owner_id FROM forge.namespaces WHERE id = $1 FOR SHARE")
            .bind(parent.get())
            .fetch_optional(conn)
            .await
            .map_err(|e| query_error("projects.create.parent_owner_kind", e))?;
    match row {
        None => Err(not_found("namespace")),
        Some((kind, owner)) if kind == "group" => Ok(ParentOwner::Group(GroupId::new(owner))),
        Some((kind, owner)) if kind == "user" => Ok(ParentOwner::User(UserId::new(owner))),
        Some(_) => Err(request_invalid("parent_not_owner_namespace")),
    }
}

/// [`create_in`]'s steps after `BEGIN`, up to the record to commit. With
/// `decide`, the decision comes first (M1 phase 8b MR7a LOW-5): a caller
/// who may not create gets the denial, never a name or field error.
async fn create_steps<D, P>(
    tx: &mut ChangeTx,
    ctx: &ChangeContext,
    new: &NewProject,
    decide: Option<D>,
) -> Result<Settle<(CreatedProject, Option<P>)>, ForgeError>
where
    D: FnOnce(&ProjectCreateFacts) -> Result<P, ForgeError>,
{
    let decided = match decide {
        None => None,
        Some(decide) => {
            let (creator, namespace) = (new.creator, new.parent);
            let owner = tx.step(|c| lock_parent_owner(c, namespace)).await?;
            let parent = match owner {
                ParentOwner::Group(group) => {
                    let roles = tx
                        .step(|c| lock_roles(c, RoleAnchor::Namespace(namespace), creator))
                        .await?;
                    ParentAccess::Group {
                        namespace,
                        group,
                        role: roles.native,
                    }
                }
                ParentOwner::User(owner) => ParentAccess::User { namespace, owner },
            };
            let account = tx.step(|c| lock_account(c, creator)).await?;
            Some(decide(&ProjectCreateFacts { account, parent })?)
        }
    };
    // Never top level, so the reserved top-level words are allowed.
    let segment = PathSegment::parse(&new.segment)?;
    validate(new)?;
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
    Ok(Settle::Commit(
        evented_record(&details, created.project_id)?,
        (created, decided),
    ))
}

/// The audit row and the event of a project change (`project.created`,
/// D4 MR4 `member.*`): both on the project (the event scope rule, ADR-0016
/// Amendment D4); the event's payload is the audit details (ids, kinds,
/// roles). No dedup key: `project.created` names a new project and each
/// member change is a new state, never a retry to absorb.
pub(crate) fn evented_record(
    details: &AuditDetails,
    project: ProjectId,
) -> Result<ChangeRecord, ForgeError> {
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

#[cfg(test)]
mod tests;
