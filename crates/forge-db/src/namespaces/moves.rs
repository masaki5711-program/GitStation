//! The one subtree move (D5 MR2, ADR-0015 "名前の変更、移管、削除と墓標" and
//! Amendment D5): a project move or rename, a group move or rename and a
//! user rename all move one namespace and its descendants with
//! [`move_subtree`], inside the caller's `ChangeTx`.
//!
//! Lock order (every command): the resource row (`projects` / `groups` /
//! `users`, `FOR UPDATE`), then the subtree's namespace rows
//! ([`lock_subtree`], `FOR UPDATE`, in id order, re-read until stable),
//! then the new parent ([`lock_target`], `FOR SHARE`) with the caller's
//! membership rows (`roles::lock_roles`, `FOR SHARE`), then the caller's
//! account — the order of `projects::members` — and only after the
//! decision the path advisory locks of every old and new path
//! ([`super::lock_paths`], in byte order). A create below any node of the
//! subtree reads its parent `FOR SHARE`, so it either commits before the
//! move locks that node (and the re-read picks the new child up) or waits
//! and then reads the moved parent's new path.
//!
//! In the same transaction, before anything is written: the target kind,
//! the same location, the owner of a personal target (`parent_owner_bot`,
//! `parent_owner_inactive`), a cycle, the depth (≤ 20), every new path's
//! length (≤ 255), the subtree size (≤ [`MAX_SUBTREE`]), the rate limit
//! ([`MAX_MOVES_PER_DAY`]) and the collisions of every new path with a live
//! namespace or a tombstone. Then one depth-capped recursive `UPDATE`
//! rewrites the subtree and one `moved` tombstone per node keeps its old
//! path (ADR-0015: per node, exact match only).

use forge_core::ids::{GroupId, NamespaceId, ProjectId, UserId};
use forge_core::namespace_path::{MAX_DEPTH, MAX_FULL_PATH_LEN};
use forge_core::TombstoneKind;
use forge_core::{AuditDetails, MoveSummary, PathSegment, Role, RoleChange, TombstoneId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{lock_paths, path_taken};
use crate::change::{Actor, AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome};
use crate::query_error;
use crate::roles::{lock_roles, RoleAnchor};

/// The most namespaces one move rewrites (the root included).
pub const MAX_SUBTREE: usize = 1000;
/// Moves of one namespace (as the cause of its `moved` tombstones) within
/// 24 hours; the 11th is `FG-REPO-0018`.
pub const MAX_MOVES_PER_DAY: i64 = 10;
/// The most role changes a move reads (more are reported as truncated).
const MAX_ROLE_CHANGES: i64 = 1000;
/// How often the subtree is re-read until the locked set is stable.
const STABLE_PASSES: usize = 3;
/// The cap of the recursive walks, as the SQL parameter.
const DEPTH_CAP: i32 = {
    assert!(MAX_DEPTH <= i32::MAX as usize);
    MAX_DEPTH as i32
};

/// Whether a move counts against [`MAX_MOVES_PER_DAY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RateLimit {
    #[default]
    Enforce,
    /// The operator CLI only (actor `system`, step D5 plan answer); any
    /// other actor is refused (`FG-SYS-0001 reason=rate_limit_exempt_not_system`).
    Exempt,
}

/// How a move treats tombstones and the rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MoveOptions {
    /// A→B→A (ADR-0015 Amendment D5): release, in the same transaction, the
    /// `moved` tombstones at the new paths that name the very namespaces
    /// moving there. A tombstone naming another namespace still blocks.
    pub reclaim: bool,
    pub rate_limit: RateLimit,
}

/// Where a namespace moves: its new parent (`None` = top level) and last
/// segment. A rename keeps the parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub parent: Option<NamespaceId>,
    pub segment: String,
}

/// The new parent as the move holds it (what `decide` needs to build
/// forge-auth's `TransferTarget`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAccess {
    TopLevel,
    /// A group namespace and the caller's native role on it.
    Group {
        namespace: NamespaceId,
        group: GroupId,
        role: Option<Role>,
    },
    /// A personal namespace and its owner.
    PersonalNamespace {
        namespace: NamespaceId,
        owner: UserId,
    },
    /// A project namespace: never a valid parent (`target_kind`).
    Project {
        namespace: NamespaceId,
    },
}

/// What a move did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutcome {
    pub old_path: String,
    pub new_path: String,
    /// The audit summary (ids, counts, inherited role changes).
    pub summary: MoveSummary,
    /// Every project whose path changed, by id.
    pub projects: Vec<ProjectId>,
}

/// A namespace row of the subtree, as locked.
#[derive(Debug, Clone)]
struct Node {
    id: i64,
    parent_id: Option<i64>,
    kind: String,
    owner_id: i64,
    segment: String,
    full_path: String,
    depth: i16,
}

/// A namespace row as read: id, parent_id, kind, owner_id, segment,
/// full_path, depth.
type NodeRow = (i64, Option<i64>, String, i64, String, String, i16);

/// The locked subtree: the root first, then by depth and id.
pub(crate) struct Subtree {
    nodes: Vec<Node>,
    too_large: bool,
}

impl Subtree {
    pub(crate) fn root_parent(&self) -> Option<NamespaceId> {
        self.nodes
            .first()
            .and_then(|n| n.parent_id)
            .map(NamespaceId::new)
    }
}

/// The new parent as locked.
pub(crate) struct Target {
    id: Option<i64>,
    path: Option<String>,
    depth: i16,
    access: TargetAccess,
    /// A personal target's owner: (kind, state).
    owner: Option<(String, String)>,
}

impl Target {
    pub(crate) fn access(&self) -> TargetAccess {
        self.access
    }
}

/// What [`move_subtree`] wrote, for the caller's record.
pub(crate) struct Moved {
    pub(crate) outcome: MoveOutcome,
    /// The audit rows besides the cause's own (one per other project, and
    /// the causes of reclaimed tombstones not covered otherwise).
    pub(crate) affected: Vec<AuditEntry>,
}

impl Moved {
    /// The change's record: `details` on the cause's resource, then the
    /// affected rows.
    pub(crate) fn record(
        &self,
        details: &AuditDetails,
        resource_type: &'static str,
        resource_id: i64,
    ) -> ChangeRecord {
        let mut record = ChangeRecord::audit_only(details, resource_type, resource_id);
        if resource_type == "project" {
            record.audit.project_id = Some(ProjectId::new(resource_id));
        }
        record.affected = self.affected.clone(); // clone: the outcome keeps its copy
        record
    }
}

/// `FG-REPO-0015` with its `reason`.
fn invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::TransferInvalid).with_context("reason", reason)
}

fn not_found(resource: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", resource)
        .with_context("reason", "absent")
}

/// The ids of the subtree below `$1`, capped in depth ($2) and count ($3).
const SUBTREE_IDS: &str = "WITH RECURSIVE sub (id, step) AS ( \
       SELECT id, 1 FROM forge.namespaces WHERE id = $1 \
     UNION ALL \
       SELECT n.id, s.step + 1 FROM forge.namespaces n JOIN sub s ON n.parent_id = s.id \
       WHERE s.step < $2) \
     SELECT id FROM sub LIMIT $3";

async fn subtree_ids(conn: &mut PgConnection, root: i64) -> Result<Vec<i64>, ForgeError> {
    let mut ids: Vec<i64> = sqlx::query_scalar(SUBTREE_IDS)
        .bind(root)
        .bind(DEPTH_CAP)
        .bind(
            i64::try_from(MAX_SUBTREE)
                .unwrap_or(i64::MAX)
                .saturating_add(1),
        )
        .fetch_all(conn)
        .await
        .map_err(|e| query_error("namespaces.move.subtree", e))?;
    ids.sort_unstable();
    Ok(ids)
}

/// Lock the namespace `root` and its descendants `FOR UPDATE`, in id
/// order, re-reading the subtree until the locked set is stable (a child
/// created before a node was locked is visible to the next read; after it,
/// no create can add one). At most `MAX_SUBTREE + 1` rows are locked; a
/// larger subtree is reported after the decision (`subtree_too_large`).
/// `FG-REPO-0005 resource=namespace` when `root` does not exist.
pub(crate) async fn lock_subtree(
    tx: &mut ChangeTx,
    root: NamespaceId,
) -> Result<Subtree, ForgeError> {
    let root = root.get();
    let mut locked: Vec<i64> = Vec::new();
    let mut stable = false;
    for _ in 0..STABLE_PASSES {
        let ids = tx.step(|c| subtree_ids(c, root)).await?;
        if ids.is_empty() {
            return Err(not_found("namespace"));
        }
        if ids == locked {
            stable = true;
            break;
        }
        let to_lock = ids.clone(); // clone: compared with the next read
        tx.step(|c| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM forge.namespaces WHERE id = ANY($1) ORDER BY id FOR UPDATE",
            )
            .bind(&to_lock)
            .fetch_all(c)
            .await
            .map_err(|e| query_error("namespaces.move.lock", e))
        })
        .await?;
        locked = ids;
        if locked.len() > MAX_SUBTREE {
            return Ok(Subtree {
                nodes: Vec::new(),
                too_large: true,
            });
        }
    }
    if !stable {
        return Err(ForgeError::new(ErrorCode::DbQueryFailed)
            .with_context("query", "namespaces.move.lock")
            .with_context("reason", "subtree_unstable"));
    }
    let ids = locked;
    let rows: Vec<NodeRow> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT id, parent_id, kind, owner_id, segment, full_path, depth \
                 FROM forge.namespaces WHERE id = ANY($1)",
            )
            .bind(&ids)
            .fetch_all(c)
            .await
            .map_err(|e| query_error("namespaces.move.read", e))
        })
        .await?;
    let mut nodes: Vec<Node> = rows
        .into_iter()
        .map(
            |(id, parent_id, kind, owner_id, segment, full_path, depth)| Node {
                id,
                parent_id,
                kind,
                owner_id,
                segment,
                full_path,
                depth,
            },
        )
        .collect();
    nodes.sort_by_key(|n| (n.id != root, n.depth, n.id));
    Ok(Subtree {
        nodes,
        too_large: false,
    })
}

/// Lock the new parent (`FOR SHARE`, so it cannot move before the commit)
/// and read what `decide` needs: for a group, `viewer`'s native role (the
/// membership rows `FOR SHARE`); for a personal namespace, its owner (the
/// user row `FOR SHARE`). `None` is the top level. `FG-REPO-0005
/// resource=namespace` when the parent does not exist.
pub(crate) async fn lock_target(
    tx: &mut ChangeTx,
    parent: Option<NamespaceId>,
    viewer: UserId,
) -> Result<Target, ForgeError> {
    let Some(parent) = parent else {
        return Ok(Target {
            id: None,
            path: None,
            depth: 0,
            access: TargetAccess::TopLevel,
            owner: None,
        });
    };
    let row: Option<(String, i64, String, i16)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT kind, owner_id, full_path, depth FROM forge.namespaces \
                 WHERE id = $1 FOR SHARE",
            )
            .bind(parent.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("namespaces.move.target", e))
        })
        .await?;
    let Some((kind, owner_id, path, depth)) = row else {
        return Err(not_found("namespace"));
    };
    let (access, owner) = match kind.as_str() {
        "group" => {
            let roles = tx
                .step(|c| lock_roles(c, RoleAnchor::Namespace(parent), viewer))
                .await?;
            (
                TargetAccess::Group {
                    namespace: parent,
                    group: GroupId::new(owner_id),
                    role: roles.native,
                },
                None,
            )
        }
        "user" => {
            let owner: Option<(String, String)> = tx
                .step(|c| async move {
                    sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR SHARE")
                        .bind(owner_id)
                        .fetch_optional(c)
                        .await
                        .map_err(|e| query_error("namespaces.move.target_owner", e))
                })
                .await?;
            let Some(owner) = owner else {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "namespace_owner_missing")
                    .with_context("user_id", owner_id));
            };
            (
                TargetAccess::PersonalNamespace {
                    namespace: parent,
                    owner: UserId::new(owner_id),
                },
                Some(owner),
            )
        }
        _ => (TargetAccess::Project { namespace: parent }, None),
    };
    Ok(Target {
        id: Some(parent.get()),
        path: Some(path),
        depth,
        access,
        owner,
    })
}

/// The inherited role ranks of the old ($1) and new ($2) parent chains,
/// per user, where they differ: group memberships of active users in every
/// ancestor group, and — for a moved project ($4) — Owner for the active,
/// non-bot owner of a personal parent (`roles::chain_role_columns`).
const ROLE_DIFF: &str = "WITH RECURSIVE chain (side, kind, owner_id, parent_id, step) AS ( \
       SELECT v.side, n.kind, n.owner_id, n.parent_id, 1 \
       FROM (VALUES ('old', $1::bigint), ('new', $2::bigint)) AS v (side, id) \
       JOIN forge.namespaces n ON n.id = v.id \
     UNION ALL \
       SELECT c.side, n.kind, n.owner_id, n.parent_id, c.step + 1 \
       FROM forge.namespaces n JOIN chain c ON n.id = c.parent_id WHERE c.step < $3), \
     grants (side, user_id, rank) AS ( \
       SELECT c.side, m.user_id, CASE m.role WHEN 'guest' THEN 1 WHEN 'reporter' THEN 2 \
         WHEN 'developer' THEN 3 WHEN 'maintainer' THEN 4 WHEN 'owner' THEN 5 END \
       FROM chain c JOIN forge.group_members m ON c.kind = 'group' AND m.group_id = c.owner_id \
       JOIN forge.users u ON u.id = m.user_id AND u.state = 'active' \
     UNION ALL \
       SELECT c.side, u.id, 5 FROM chain c JOIN forge.users u ON c.kind = 'user' \
         AND c.step = 1 AND u.id = c.owner_id AND u.state = 'active' AND u.kind <> 'bot' \
       WHERE $4), \
     best AS (SELECT user_id, max(rank) FILTER (WHERE side = 'old') AS old_rank, \
       max(rank) FILTER (WHERE side = 'new') AS new_rank FROM grants GROUP BY user_id) \
     SELECT user_id, old_rank, new_rank FROM best \
     WHERE old_rank IS DISTINCT FROM new_rank ORDER BY user_id LIMIT $5";

fn rank_role(rank: Option<i32>) -> Option<Role> {
    match rank {
        Some(1) => Some(Role::Guest),
        Some(2) => Some(Role::Reporter),
        Some(3) => Some(Role::Developer),
        Some(4) => Some(Role::Maintainer),
        Some(5) => Some(Role::Owner),
        _ => None,
    }
}

/// The static audit resource type of a namespace kind.
fn resource_of(kind: &str) -> Result<&'static str, ForgeError> {
    match kind {
        "user" => Ok("user"),
        "group" => Ok("group"),
        "project" => Ok("project"),
        _ => Err(ForgeError::new(ErrorCode::NamespaceDataInvalid)
            .with_context("reason", "unknown_namespace_kind")),
    }
}

/// An audit row on `(resource_type, id)`, naming the project for a project.
fn affected_row(details: &AuditDetails, resource_type: &'static str, id: i64) -> AuditEntry {
    AuditEntry {
        action: details.action(),
        outcome: Outcome::Succeeded,
        resource_type,
        resource_id: Some(id),
        project_id: (resource_type == "project").then(|| ProjectId::new(id)),
        error_code: None,
        details: details.to_value(),
    }
}

/// Is the target kind one the moved kind may go under?
fn target_fits(kind: &str, target: &TargetAccess) -> bool {
    match kind {
        "project" => matches!(
            target,
            TargetAccess::Group { .. } | TargetAccess::PersonalNamespace { .. }
        ),
        "group" => matches!(target, TargetAccess::Group { .. } | TargetAccess::TopLevel),
        "user" => matches!(target, TargetAccess::TopLevel),
        _ => false,
    }
}

/// Move the locked `subtree` below the locked `target` as `segment` (module
/// docs), after the caller's decision. Errors (nothing is written; the
/// caller settles): `FG-REPO-0015` (`subtree_too_large`, `target_kind`,
/// `same_location`, `parent_owner_bot`, `parent_owner_inactive`, `cycle`,
/// `too_deep`, `path_too_long`), `FG-REPO-0007` / `0008` the segment,
/// `FG-REPO-0018` the rate limit, `FG-REPO-0009` a new path that is taken
/// (`sibling`) or tombstoned (`tombstoned`, unless reclaimed).
pub(crate) async fn move_subtree(
    tx: &mut ChangeTx,
    ctx: &ChangeContext,
    subtree: &Subtree,
    target: &Target,
    segment: &str,
    options: MoveOptions,
) -> Result<Moved, ForgeError> {
    if subtree.too_large {
        return Err(invalid("subtree_too_large").with_context("limit", MAX_SUBTREE));
    }
    let Some(root) = subtree.nodes.first() else {
        return Err(not_found("namespace"));
    };
    if !target_fits(&root.kind, &target.access) {
        return Err(invalid("target_kind"));
    }
    let segment = match target.access {
        TargetAccess::TopLevel => PathSegment::parse_top_level(segment)?,
        TargetAccess::Group { .. }
        | TargetAccess::PersonalNamespace { .. }
        | TargetAccess::Project { .. } => PathSegment::parse(segment)?,
    };
    if target.id == root.parent_id && segment.as_str() == root.segment {
        return Err(invalid("same_location"));
    }
    if let Some((kind, state)) = &target.owner {
        if kind == "bot" {
            return Err(invalid("parent_owner_bot"));
        }
        if state != "active" {
            return Err(invalid("parent_owner_inactive"));
        }
    }
    if target
        .id
        .is_some_and(|t| subtree.nodes.iter().any(|n| n.id == t))
    {
        return Err(invalid("cycle"));
    }
    let deepest = subtree
        .nodes
        .iter()
        .map(|n| n.depth)
        .max()
        .unwrap_or(root.depth);
    let new_root_depth = target.depth.saturating_add(1);
    let new_deepest = i32::from(new_root_depth) + i32::from(deepest) - i32::from(root.depth);
    if new_deepest > DEPTH_CAP {
        return Err(invalid("too_deep").with_context("len", new_deepest));
    }
    let new_root_path = match &target.path {
        Some(parent) => format!("{parent}/{}", segment.as_str()),
        None => segment.as_str().to_owned(),
    };
    let old_root_len = root.full_path.len();
    let mut new_paths = Vec::with_capacity(subtree.nodes.len());
    for node in &subtree.nodes {
        let suffix = node.full_path.get(old_root_len..).ok_or_else(|| {
            ForgeError::new(ErrorCode::NamespaceDataInvalid)
                .with_context("reason", "subtree_path_mismatch")
                .with_context("namespace_id", node.id)
        })?;
        new_paths.push(format!("{new_root_path}{suffix}"));
    }
    if let Some(len) = new_paths
        .iter()
        .map(|p| p.chars().count())
        .max()
        .filter(|l| *l > MAX_FULL_PATH_LEN)
    {
        return Err(invalid("path_too_long").with_context("len", len));
    }
    let cause_type = resource_of(&root.kind)?;
    let cause_id = root.owner_id;
    check_rate(tx, ctx, cause_type, cause_id, options.rate_limit).await?;
    let old_paths: Vec<String> = subtree.nodes.iter().map(|n| n.full_path.clone()).collect();
    let ids: Vec<i64> = subtree.nodes.iter().map(|n| n.id).collect();
    let all_paths: Vec<String> = old_paths.iter().chain(new_paths.iter()).cloned().collect();
    tx.step(|c| lock_paths(c, &all_paths)).await?;
    let (paths, others) = (&new_paths, &ids);
    let live: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "SELECT count(*) FROM forge.namespaces \
                 WHERE full_path = ANY($1) AND NOT (id = ANY($2))",
            )
            .bind(paths)
            .bind(others)
            .fetch_one(c)
            .await
            .map_err(|e| query_error("namespaces.move.collisions", e))
        })
        .await?;
    if live > 0 {
        return Err(path_taken("sibling"));
    }
    let reclaimed = reclaim(tx, subtree, &new_paths, options.reclaim).await?;
    let role_changes = if target.id == root.parent_id {
        (Vec::new(), false)
    } else {
        role_diff(tx, root.parent_id, target.id, root.kind == "project").await?
    };
    let delta = new_root_depth.saturating_sub(root.depth);
    let (root_id, parent, new_root, old_len) = (
        root.id,
        target.id,
        new_root_path.as_str(),
        i32::try_from(old_root_len)
            .unwrap_or(i32::MAX)
            .saturating_add(1),
    );
    let segment_str = segment.as_str();
    let updated = tx
        .step(|c| async move {
            sqlx::query(
                "WITH RECURSIVE sub (id, step) AS ( \
                   SELECT $1::bigint, 1 \
                 UNION ALL \
                   SELECT n.id, s.step + 1 FROM forge.namespaces n \
                   JOIN sub s ON n.parent_id = s.id WHERE s.step < $2) \
                 UPDATE forge.namespaces AS n SET \
                   parent_id = CASE WHEN n.id = $1 THEN $3::bigint ELSE n.parent_id END, \
                   segment = CASE WHEN n.id = $1 THEN $4 ELSE n.segment END, \
                   full_path = $5 || substr(n.full_path, $6), \
                   depth = n.depth + $7 \
                 FROM sub WHERE n.id = sub.id",
            )
            .bind(root_id)
            .bind(DEPTH_CAP)
            .bind(parent)
            .bind(segment_str)
            .bind(new_root)
            .bind(old_len)
            .bind(delta)
            .execute(c)
            .await
            .map_err(
                |e| match e.as_database_error().and_then(|d| d.constraint()) {
                    Some("namespaces_sibling_key" | "namespaces_full_path_key") => {
                        path_taken("sibling")
                    }
                    _ => query_error("namespaces.move.update", e),
                },
            )
        })
        .await?
        .rows_affected();
    if usize::try_from(updated).ok() != Some(subtree.nodes.len()) {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "subtree_changed_during_move")
            .with_context("locked", subtree.nodes.len())
            .with_context("updated", updated));
    }
    let parents: Vec<Option<i64>> = subtree.nodes.iter().map(|n| n.parent_id).collect();
    let owner_at_write = ctx.actor.user();
    let (old, nodes) = (&old_paths, &ids);
    tx.step(|c| async move {
        sqlx::query(
            "INSERT INTO forge.namespace_tombstones (old_path, parent_namespace_id, \
             namespace_id, kind, owner_at_write, cause_type, cause_id) \
             SELECT t.p, t.parent, t.id, 'moved', $4, $5, $6 \
             FROM unnest($1::text[], $2::bigint[], $3::bigint[]) AS t (p, parent, id)",
        )
        .bind(old)
        .bind(&parents)
        .bind(nodes)
        .bind(owner_at_write)
        .bind(cause_type)
        .bind(cause_id)
        .execute(c)
        .await
        .map_err(|e| query_error("namespaces.move.tombstones", e))
    })
    .await?;
    let projects: Vec<ProjectId> = subtree
        .nodes
        .iter()
        .filter(|n| n.kind == "project")
        .map(|n| ProjectId::new(n.owner_id))
        .collect();
    let mut covered: Vec<(&'static str, i64)> = vec![(cause_type, cause_id)];
    let mut affected = Vec::new();
    for node in subtree.nodes.iter().filter(|n| n.kind == "project") {
        if (cause_type, cause_id) == ("project", node.owner_id) {
            continue;
        }
        let details = AuditDetails::ProjectMovedWithAncestor {
            project_id: ProjectId::new(node.owner_id),
            namespace_id: NamespaceId::new(node.id),
            moved_namespace_id: NamespaceId::new(root.id),
        };
        affected.push(affected_row(&details, "project", node.owner_id));
        covered.push(("project", node.owner_id));
    }
    for r in &reclaimed {
        if covered.contains(&(r.cause_type, r.cause_id)) {
            continue;
        }
        let details = AuditDetails::TombstoneReleased {
            tombstone_id: TombstoneId::new(r.id),
            kind: TombstoneKind::Moved,
            namespace_id: Some(NamespaceId::new(r.namespace_id)),
            reclaim: true,
        };
        affected.push(affected_row(&details, r.cause_type, r.cause_id));
        covered.push((r.cause_type, r.cause_id));
    }
    let summary = MoveSummary {
        namespace_id: NamespaceId::new(root.id),
        from_parent: root.parent_id.map(NamespaceId::new),
        to_parent: target.id.map(NamespaceId::new),
        renamed: segment.as_str() != root.segment,
        nodes: i64::try_from(subtree.nodes.len()).unwrap_or(i64::MAX),
        projects: i64::try_from(projects.len()).unwrap_or(i64::MAX),
        reclaimed_tombstones: i64::try_from(reclaimed.len()).unwrap_or(i64::MAX),
        role_changes: role_changes.0,
        role_changes_truncated: role_changes.1,
    };
    Ok(Moved {
        outcome: MoveOutcome {
            old_path: root.full_path.clone(),
            new_path: new_root_path,
            summary,
            projects,
        },
        affected,
    })
}

/// `FG-REPO-0018` when the cause already moved [`MAX_MOVES_PER_DAY`] times
/// in 24 hours, counted from its `moved` tombstones (one move = one
/// transaction = one `created_at`). The resource row is locked, so two
/// moves of it never count concurrently.
async fn check_rate(
    tx: &mut ChangeTx,
    ctx: &ChangeContext,
    cause_type: &'static str,
    cause_id: i64,
    limit: RateLimit,
) -> Result<(), ForgeError> {
    if limit == RateLimit::Exempt {
        return if ctx.actor == Actor::System {
            Ok(())
        } else {
            Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "rate_limit_exempt_not_system"))
        };
    }
    let moves: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "SELECT count(DISTINCT created_at) FROM forge.namespace_tombstones \
                 WHERE cause_type = $1 AND cause_id = $2 AND kind = 'moved' \
                 AND created_at > now() - interval '24 hours'",
            )
            .bind(cause_type)
            .bind(cause_id)
            .fetch_one(c)
            .await
            .map_err(|e| query_error("namespaces.move.rate", e))
        })
        .await?;
    if moves >= MAX_MOVES_PER_DAY {
        return Err(ForgeError::new(ErrorCode::RenameRateLimited)
            .with_context("reason", "moves_per_day")
            .with_context("moves", moves)
            .with_context("limit", MAX_MOVES_PER_DAY)
            .with_context("resource", cause_type)
            .with_context("id", cause_id));
    }
    Ok(())
}

/// A tombstone released by a reclaiming move.
struct Reclaimed {
    id: i64,
    namespace_id: i64,
    cause_type: &'static str,
    cause_id: i64,
}

/// The tombstones at the new paths: none, or — with `reclaim` — only
/// `moved` tombstones naming the namespace that takes their path, which are
/// deleted. Anything else is `FG-REPO-0009 reason=tombstoned`.
async fn reclaim(
    tx: &mut ChangeTx,
    subtree: &Subtree,
    new_paths: &[String],
    reclaim: bool,
) -> Result<Vec<Reclaimed>, ForgeError> {
    let rows: Vec<(i64, String, Option<i64>, String, String, i64)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT id, old_path, namespace_id, kind, cause_type, cause_id \
                 FROM forge.namespace_tombstones WHERE old_path = ANY($1) ORDER BY id",
            )
            .bind(new_paths)
            .fetch_all(c)
            .await
            .map_err(|e| query_error("namespaces.move.tombstoned", e))
        })
        .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    if !reclaim {
        return Err(path_taken("tombstoned"));
    }
    let mut released = Vec::with_capacity(rows.len());
    for (id, old_path, namespace_id, kind, cause_type, cause_id) in rows {
        let taker = new_paths
            .iter()
            .position(|p| *p == old_path)
            .and_then(|i| subtree.nodes.get(i))
            .map(|n| n.id);
        match namespace_id {
            Some(ns) if kind == "moved" && taker == Some(ns) => released.push(Reclaimed {
                id,
                namespace_id: ns,
                cause_type: resource_of(&cause_type)?,
                cause_id,
            }),
            _ => return Err(path_taken("tombstoned")),
        }
    }
    let ids: Vec<i64> = released.iter().map(|r| r.id).collect();
    let deleted = tx
        .step(|c| async move {
            sqlx::query("DELETE FROM forge.namespace_tombstones WHERE id = ANY($1)")
                .bind(&ids)
                .execute(c)
                .await
                .map_err(|e| query_error("namespaces.move.reclaim", e))
        })
        .await?
        .rows_affected();
    if usize::try_from(deleted).ok() != Some(released.len()) {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "reclaim_changed_during_move"));
    }
    Ok(released)
}

/// The users whose inherited role differs between the old and the new
/// parent chain: at most [`MAX_ROLE_CHANGES`], and whether there are more.
async fn role_diff(
    tx: &mut ChangeTx,
    old_parent: Option<i64>,
    new_parent: Option<i64>,
    is_project: bool,
) -> Result<(Vec<RoleChange>, bool), ForgeError> {
    let rows: Vec<(i64, Option<i32>, Option<i32>)> = tx
        .step(|c| async move {
            sqlx::query_as(ROLE_DIFF)
                .bind(old_parent)
                .bind(new_parent)
                .bind(DEPTH_CAP)
                .bind(is_project)
                .bind(MAX_ROLE_CHANGES.saturating_add(1))
                .fetch_all(c)
                .await
                .map_err(|e| query_error("namespaces.move.role_diff", e))
        })
        .await?;
    let truncated = i64::try_from(rows.len()).unwrap_or(i64::MAX) > MAX_ROLE_CHANGES;
    let changes = rows
        .into_iter()
        .take(usize::try_from(MAX_ROLE_CHANGES).unwrap_or(usize::MAX))
        .map(|(user, from, to)| RoleChange {
            user: UserId::new(user),
            from: rank_role(from),
            to: rank_role(to),
        })
        .collect();
    Ok((changes, truncated))
}
