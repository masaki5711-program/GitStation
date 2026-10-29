//! Group commands (step D2, ADR-0010, ADR-0015, ADR-0016 §6): create a group
//! with its namespace and first Owner, and change its direct members
//! ([`members`]). Each command is one `ChangeTx` with one `group` audit row
//! (`forge.audited_change_present` refuses anything else); `group.*` and
//! `member.*` are audit-only (step D plan Q4, Q8). Groups are not deleted in
//! M1 (Q11).

use forge_core::ids::{GroupId, NamespaceId, UserId};
use forge_core::namespace_path::{MAX_DEPTH, MAX_FULL_PATH_LEN};
use forge_core::{AuditDetails, PathSegment};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::change::{ChangeContext, ChangeRecord, ChangeTx};
use crate::namespaces::{is_tombstoned, lock_path, path_taken};
use crate::{query_error, Db};

mod members;

pub use members::{add_member, change_member, remove_member, MemberChange};

/// The audit resource type of groups and their members (tables.toml).
const RESOURCE: &str = "group";

/// The request to create a group. `segment` is the last path segment; the
/// full path is the parent's path plus it.
#[derive(Debug, Clone)]
pub struct NewGroup {
    pub segment: String,
    /// The parent group's namespace; `None` = a top-level group (its segment
    /// must not be a reserved word).
    pub parent: Option<NamespaceId>,
    pub display_name: String,
    pub description: String,
    /// The first Owner: an active human or service account.
    pub owner: UserId,
}

/// What `create` wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatedGroup {
    pub group_id: GroupId,
    pub namespace_id: NamespaceId,
}

fn request_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GroupRequestInvalid).with_context("reason", reason)
}

/// `FG-REPO-0010` with its `reason`.
fn membership_conflict(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::MembershipConflict).with_context("reason", reason)
}

fn not_found(resource: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", resource)
        .with_context("reason", "absent")
}

/// The field rules of forge.groups (its CHECKs, migration 20261029100000),
/// checked before anything is written: `FG-REPO-0011` with `reason` / `len`.
fn validate(new: &NewGroup) -> Result<(), ForgeError> {
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

/// The user who may become a member: present, not a bot (`FG-REPO-0010`
/// `bot_member`). Returns whether the account is active. The row is read
/// `FOR SHARE`, so it cannot be disabled before the change commits.
async fn member_candidate(conn: &mut PgConnection, user: UserId) -> Result<bool, ForgeError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR SHARE")
            .bind(user.get())
            .fetch_optional(conn)
            .await
            .map_err(|e| query_error("groups.member_candidate", e))?;
    let Some((kind, state)) = row else {
        return Err(not_found("user"));
    };
    if kind == "bot" {
        return Err(membership_conflict("bot_member"));
    }
    Ok(state == "active")
}

/// A unique violation on the path is a taken path; anything else stays a
/// query error.
fn namespace_conflict(e: sqlx::Error) -> ForgeError {
    match e.as_database_error().and_then(|d| d.constraint()) {
        Some("namespaces_sibling_key" | "namespaces_full_path_key") => path_taken("sibling"),
        _ => query_error("groups.create.namespace", e),
    }
}

/// Where the new group's namespace goes: its full path, depth and parent.
struct Placement {
    full_path: String,
    depth: i16,
    parent: Option<NamespaceId>,
}

/// The parent must be a group (groups nest only under groups, ADR-0015
/// Amendment D2), read `FOR SHARE` so it cannot move before the commit; the
/// result must stay within `MAX_DEPTH` and `MAX_FULL_PATH_LEN`
/// (`FG-REPO-0007` `too_deep` / `path_too_long`).
async fn place(
    conn: &mut PgConnection,
    segment: &PathSegment,
    parent: Option<NamespaceId>,
) -> Result<Placement, ForgeError> {
    let Some(parent) = parent else {
        return Ok(Placement {
            full_path: segment.as_str().to_owned(),
            depth: 1,
            parent: None,
        });
    };
    let row: Option<(String, String, i16)> = sqlx::query_as(
        "SELECT kind, full_path, depth FROM forge.namespaces WHERE id = $1 FOR SHARE",
    )
    .bind(parent.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("groups.create.parent", e))?;
    let Some((kind, parent_path, parent_depth)) = row else {
        return Err(not_found("namespace"));
    };
    if kind != "group" {
        return Err(request_invalid("parent_not_group"));
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
    Ok(Placement {
        full_path,
        depth,
        parent: Some(parent),
    })
}

/// Insert the group, its namespace (`kind = 'group'`, `owner_id =
/// groups.id`) and its first Owner, with one `group.created` audit row
/// (resource `group`/`groups.id`); no event. The path is taken under its
/// advisory lock (`namespaces::lock_path`) and checked against tombstones
/// before and after the insert, as `users::create` does.
///
/// Errors (nothing is written): `FG-REPO-0007` invalid name (also
/// `too_deep`, `path_too_long`), `FG-REPO-0008` reserved top-level name,
/// `FG-REPO-0009` taken / tombstoned, `FG-REPO-0011` a field, a parent that
/// is not a group or an inactive owner, `FG-REPO-0010` `bot_member`,
/// `FG-REPO-0005` unknown parent namespace or owner (`reason=absent`).
pub async fn create(
    db: &Db,
    ctx: &ChangeContext,
    new: &NewGroup,
) -> Result<CreatedGroup, ForgeError> {
    let segment = match new.parent {
        None => PathSegment::parse_top_level(&new.segment)?,
        Some(_) => PathSegment::parse(&new.segment)?,
    };
    validate(new)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let placement = tx.step(|c| place(c, &segment, new.parent)).await?;
    if !tx.step(|c| member_candidate(c, new.owner)).await? {
        return Err(request_invalid("owner_inactive"));
    }
    let path = placement.full_path.as_str();
    tx.step(|c| lock_path(c, path)).await?;
    if tx.step(|c| is_tombstoned(c, path)).await? {
        return Err(path_taken("tombstoned"));
    }
    let group_id: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "INSERT INTO forge.groups (display_name, description) VALUES ($1, $2) RETURNING id",
            )
            .bind(new.display_name.as_str())
            .bind(new.description.as_str())
            .fetch_one(c)
            .await
            .map_err(|e| query_error("groups.create.group", e))
        })
        .await?;
    let (parent, depth) = (placement.parent, placement.depth);
    let namespace_id: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "INSERT INTO forge.namespaces (parent_id, kind, owner_id, segment, full_path, \
                 depth) VALUES ($1, 'group', $2, $3, $4, $5) RETURNING id",
            )
            .bind(parent.map(NamespaceId::get))
            .bind(group_id)
            .bind(segment.as_str())
            .bind(path)
            .bind(depth)
            .fetch_one(c)
            .await
            .map_err(namespace_conflict)
        })
        .await?;
    // Defence in depth, as in users::create: the lock rule is the guarantee.
    if tx.step(|c| is_tombstoned(c, path)).await? {
        return Err(path_taken("tombstoned"));
    }
    let owner = new.owner;
    tx.step(|c| async move {
        sqlx::query(
            "INSERT INTO forge.group_members (group_id, user_id, role) VALUES ($1, $2, 'owner')",
        )
        .bind(group_id)
        .bind(owner.get())
        .execute(c)
        .await
        .map_err(|e| query_error("groups.create.owner", e))
    })
    .await?;
    let created = CreatedGroup {
        group_id: GroupId::new(group_id),
        namespace_id: NamespaceId::new(namespace_id),
    };
    let details = AuditDetails::GroupCreated {
        namespace_id: created.namespace_id,
        parent_namespace_id: placement.parent,
        owner,
    };
    tx.commit(ChangeRecord::audit_only(&details, RESOURCE, group_id))
        .await?
        .require_recorded(details.action())?;
    Ok(created)
}

#[cfg(test)]
mod tests;
