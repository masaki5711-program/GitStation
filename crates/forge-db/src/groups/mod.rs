//! Group commands (step D2, ADR-0010, ADR-0015, ADR-0016 §6): create a group
//! with its namespace and first Owner, and change its direct members
//! ([`members`]). Each command is one `ChangeTx` with one `group` audit row
//! (`forge.audited_change_present` refuses anything else); `group.*` and
//! `member.*` are audit-only (step D plan Q4, Q8). Groups are not deleted in
//! M1 (Q11).

use forge_core::ids::{GroupId, NamespaceId, UserId};
use forge_core::namespace_path::{MAX_DEPTH, MAX_FULL_PATH_LEN};
use forge_core::{AuditDetails, ChangedField, PathSegment, Role};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::access::{lock_account, Account};
use crate::change::{ChangeContext, ChangeRecord, ChangeTx};
use crate::namespaces::{is_tombstoned, lock_path, path_taken};
use crate::roles::{lock_roles, RoleAnchor};
use crate::{query_error, Db};

mod members;
mod read;

pub use members::{add_member, change_member, remove_member, MemberChange};
pub use read::load;

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
    check_fields(
        Some(&new.display_name),
        Some(&new.description),
        request_invalid,
    )
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
    let segment = parse_segment(new)?;
    validate(new)?;
    let tx = ChangeTx::begin(db, ctx).await?;
    finish_create(tx, new, &segment).await
}

fn parse_segment(new: &NewGroup) -> Result<PathSegment, ForgeError> {
    match new.parent {
        None => PathSegment::parse_top_level(&new.segment),
        Some(_) => PathSegment::parse(&new.segment),
    }
}

/// Everything of [`create`] after `BEGIN` (shared with [`create_decided`]).
async fn finish_create(
    mut tx: ChangeTx,
    new: &NewGroup,
    segment: &PathSegment,
) -> Result<CreatedGroup, ForgeError> {
    let placement = tx.step(|c| place(c, segment, new.parent)).await?;
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

// ── M1 phase 8b MR7a: writes decided inside their transaction ──

/// A group as a write transaction holds it: its id, its namespace and the
/// caller's role on it (re-read after locking the membership rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupAccess {
    pub group: GroupId,
    pub namespace: NamespaceId,
    /// The caller's native role (`roles::MemberRoles::native`).
    pub role: Option<Role>,
}

/// Where [`create_decided`] puts the group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupParent {
    TopLevel,
    /// Below this group (its namespace is looked up and locked).
    Group(GroupId),
}

/// What [`create_decided`]'s `decide` sees: the creator's account and, for
/// a subgroup, the parent group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupCreateFacts {
    pub account: Account,
    pub parent: Option<GroupAccess>,
}

/// The fields of a new group (the parent and the first Owner come apart).
#[derive(Debug, Clone)]
pub struct GroupFields {
    pub segment: String,
    pub display_name: String,
    pub description: String,
}

/// Lock the namespace of `group` (`FOR SHARE`, as `place` does) and return
/// its id; `FG-REPO-0005 resource=group` when the group has none.
async fn lock_group_namespace(
    conn: &mut PgConnection,
    group: GroupId,
) -> Result<NamespaceId, ForgeError> {
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM forge.namespaces WHERE kind = 'group' AND owner_id = $1 FOR SHARE",
    )
    .bind(group.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("groups.parent_namespace", e))?;
    id.map(NamespaceId::new).ok_or_else(|| not_found("group"))
}

/// [`create`] with the authorization decided **inside** the transaction:
/// after `BEGIN`, the parent group's namespace is locked, the creator's
/// role on it re-read under `FOR SHARE` on the membership rows, and the
/// creator's account row locked; `decide` (forge-hosting:
/// `Authorized::reconfirm`) sees those facts and its error rolls everything
/// back. `owner` is the creator (the acting user) and becomes the first
/// Owner. Errors as [`create`], plus whatever `decide` returns.
pub async fn create_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    parent: GroupParent,
    fields: &GroupFields,
    owner: UserId,
    decide: impl FnOnce(&GroupCreateFacts) -> Result<P, ForgeError>,
) -> Result<(CreatedGroup, P), ForgeError> {
    let mut new = NewGroup {
        segment: fields.segment.clone(), // clone: NewGroup owns its fields
        parent: None,
        display_name: fields.display_name.clone(),
        description: fields.description.clone(),
        owner,
    };
    let segment = match parent {
        GroupParent::TopLevel => PathSegment::parse_top_level(&new.segment)?,
        GroupParent::Group(_) => PathSegment::parse(&new.segment)?,
    };
    validate(&new)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let parent_access = match parent {
        GroupParent::TopLevel => None,
        GroupParent::Group(group) => {
            let namespace = tx.step(|c| lock_group_namespace(c, group)).await?;
            let roles = tx
                .step(|c| lock_roles(c, RoleAnchor::Group(group), owner))
                .await?;
            new.parent = Some(namespace);
            Some(GroupAccess {
                group,
                namespace,
                role: roles.native,
            })
        }
    };
    let account = tx.step(|c| lock_account(c, owner)).await?;
    let decided = decide(&GroupCreateFacts {
        account,
        parent: parent_access,
    })?;
    let created = finish_create(tx, &new, &segment).await?;
    Ok((created, decided))
}

/// A change to a group's display name and / or description (`None` =
/// unchanged).
#[derive(Debug, Clone, Default)]
pub struct GroupPatch {
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// What an update did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Updated {
    /// Written, with one audit row naming these fields.
    Changed(Vec<ChangedField>),
    /// Every given value equals the stored one: nothing written.
    Unchanged,
}

/// What [`update_decided`]'s `decide` sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupUpdateFacts {
    pub account: Account,
    pub group: GroupAccess,
}

/// The patch's field rules (those of `validate`): `FG-REPO-0011` with
/// `reason` (`empty_update` when it names no field).
fn validate_patch(patch: &GroupPatch) -> Result<(), ForgeError> {
    if patch.display_name.is_none() && patch.description.is_none() {
        return Err(request_invalid("empty_update"));
    }
    check_fields(
        patch.display_name.as_deref(),
        patch.description.as_deref(),
        request_invalid,
    )
}

/// The display name / description rules of groups and projects (their
/// CHECKs): `bad(reason)` with `len`.
pub(crate) fn check_fields(
    display_name: Option<&str>,
    description: Option<&str>,
    bad: fn(&'static str) -> ForgeError,
) -> Result<(), ForgeError> {
    let fields = [
        (display_name, 1, 255, "display_name_length"),
        (description, 0, 2000, "description_length"),
    ];
    for (value, min, max, reason) in fields {
        let Some(value) = value else { continue };
        let len = value.chars().count();
        if len < min || len > max {
            return Err(bad(reason).with_context("len", len));
        }
        // PostgreSQL text cannot hold NUL; refuse it here, not as a query error.
        if value.contains('\0') {
            return Err(bad("nul_character").with_context("len", len));
        }
    }
    Ok(())
}

/// The fields of a patch whose value differs from the stored one.
pub(crate) fn changed_fields(
    patch_display: Option<&str>,
    patch_description: Option<&str>,
    stored_display: &str,
    stored_description: &str,
) -> Vec<ChangedField> {
    let mut out = Vec::new();
    if patch_display.is_some_and(|v| v != stored_display) {
        out.push(ChangedField::DisplayName);
    }
    if patch_description.is_some_and(|v| v != stored_description) {
        out.push(ChangedField::Description);
    }
    out
}

/// Change a group's display name and / or description, deciding inside the
/// transaction: the group row is locked `FOR UPDATE` (as the member
/// commands do), then the caller's (`viewer`) roles under `FOR SHARE` on
/// the membership rows, then the account row; `decide` sees those facts.
/// One `group.updated` audit row (audit-only in M1) naming the changed
/// fields, or [`Updated::Unchanged`] when nothing differs (nothing
/// written). Errors: `FG-REPO-0011` (a field, `empty_update`),
/// `FG-REPO-0005 resource=group`, and whatever `decide` returns.
pub async fn update_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    group: GroupId,
    viewer: UserId,
    patch: &GroupPatch,
    decide: impl FnOnce(&GroupUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(Updated, P), ForgeError> {
    validate_patch(patch)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let stored: Option<(String, String, i64)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT g.display_name, g.description, n.id FROM forge.groups g \
                 JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = g.id \
                 WHERE g.id = $1 FOR UPDATE OF g",
            )
            .bind(group.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("groups.update.lock", e))
        })
        .await?;
    let Some((display_name, description, namespace)) = stored else {
        return Err(not_found("group"));
    };
    let roles = tx
        .step(|c| lock_roles(c, RoleAnchor::Group(group), viewer))
        .await?;
    let account = tx.step(|c| lock_account(c, viewer)).await?;
    let decided = decide(&GroupUpdateFacts {
        account,
        group: GroupAccess {
            group,
            namespace: NamespaceId::new(namespace),
            role: roles.native,
        },
    })?;
    let fields = changed_fields(
        patch.display_name.as_deref(),
        patch.description.as_deref(),
        &display_name,
        &description,
    );
    if fields.is_empty() {
        return Ok((Updated::Unchanged, decided));
    }
    tx.step(|c| async move {
        sqlx::query(
            "UPDATE forge.groups SET display_name = coalesce($2, display_name), \
             description = coalesce($3, description), updated_at = now() WHERE id = $1",
        )
        .bind(group.get())
        .bind(patch.display_name.as_deref())
        .bind(patch.description.as_deref())
        .execute(c)
        .await
        .map_err(|e| query_error("groups.update", e))
    })
    .await?;
    let details = AuditDetails::GroupUpdated {
        group_id: group,
        fields: fields.clone(), // clone: the caller gets the list too
    };
    tx.commit(ChangeRecord::audit_only(&details, RESOURCE, group.get()))
        .await?
        .require_recorded(details.action())?;
    Ok((Updated::Changed(fields), decided))
}

// ── end M1 phase 8b MR7a ──

#[cfg(test)]
mod tests;
