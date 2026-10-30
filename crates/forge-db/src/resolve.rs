//! Path → namespace + the viewer's authorization inputs (M1 phase 8 MR3,
//! ADR-0015 "判定の順序" steps 5–6 and Amendment MR3): forge-db's
//! [`forge_core::NamespaceResolver`] and [`forge_core::ProjectAccessLoader`],
//! implemented on [`Db`] (`forge serve` injects it, as for the
//! authenticator).
//!
//! **One statement per call, whatever the answer.** [`resolve`] runs
//! [`RESOLVE_SQL`] and [`project_access`] runs [`PROJECT_ACCESS_SQL`]
//! exactly once, on the read pool under [`Db::within`] (the session's
//! `statement_timeout` plus the client-side bound). Each statement always
//! returns exactly one row (an aggregate without `GROUP BY` drives it), so
//! a live, moved, deleted, absent, visible or invisible target, a signed-in
//! or an anonymous viewer (`$2` NULL) issue the same statement, once — no
//! early return skips or adds one (ADR-0015 step 6). What is constant is
//! the number and the text of the statements, **not the work inside**: an
//! absent path stops after two index probes, a deep project walks its
//! parent chain. That residual timing difference is accepted for now
//! (ADR-0015 Amendment MR3; mitigations: resolve rate limiting in M2 and,
//! if needed, a response-time floor in forge-web). Visibility is never
//! decided here: the raw facts go to `authorize()` (forge-web builds
//! forge-auth's contexts from them).
//!
//! The viewer's role comes from the role loaders' own chain
//! (`roles::chain_cte!`, `roles::chain_role_columns!`, `roles::fold_chain`),
//! so it is the same computation as `roles::load_project_roles` /
//! `load_group_roles` (`native`).

use forge_core::ids::{GroupId, NamespaceId, ProjectId, TombstoneId, UserId};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    LookupPath, NamespaceKind, NamespacePath, ProjectAccess, Resolution, Resolved, ResolvedTarget,
    Visibility,
};
use forge_errors::{ErrorCode, ForgeError};

use crate::roles::{chain_cte, chain_role_columns, fold_chain, MemberRoles, CAP};
use crate::{read_query_error, Db};

/// `$1` the folded path (`LookupPath::full_path`), `$2` the viewer (NULL =
/// anonymous), `$3` the chain cap.
///
/// - `live`: the exact live match (`full_path` is `COLLATE "C"`, unique).
/// - `tomb`: only when there is no live match, the exact tombstone match
///   (`old_path` unique) — never a prefix, never a chain of tombstones.
/// - `target`: the live namespace, else a `moved` tombstone's namespace.
/// - `chain` / `steps`: the target's parent chain with the viewer's role at
///   each step (the role loaders' CTE).
/// - `agg`: one row — the steps, their roles, and the target's path
///   rebuilt from the segments (top first); `reaches_top` = the chain ends
///   at a top-level namespace.
/// - The result confirms the rebuilt path by an exact match on the live
///   paths with the target's id (`confirmed`), and joins the target's row
///   and, for a project, its `projects` row (`LEFT JOIN`s: absent stays one
///   row of NULLs).
pub(crate) const RESOLVE_SQL: &str = concat!(
    "WITH RECURSIVE \
       live AS (SELECT id FROM forge.namespaces WHERE full_path = $1), \
       tomb AS (SELECT id, kind, namespace_id FROM forge.namespace_tombstones \
         WHERE old_path = $1 AND NOT EXISTS (SELECT 1 FROM live)), \
       target AS (SELECT id FROM live UNION ALL \
         SELECT namespace_id FROM tomb WHERE kind = 'moved' AND namespace_id IS NOT NULL), ",
    chain_cte!("id = (SELECT id FROM target)"),
    ", steps AS (SELECT c.segment, c.parent_id AS up, ",
    chain_role_columns!(),
    " FROM chain c), \
       agg AS (SELECT array_agg(step ORDER BY step) AS steps, \
         array_agg(truncated ORDER BY step) AS truncs, \
         array_agg(role ORDER BY step) AS roles, \
         string_agg(segment, '/' ORDER BY step DESC) AS rebuilt, \
         coalesce(bool_or(truncated), false) AS truncated, \
         coalesce(bool_or(up IS NULL), false) AS reaches_top FROM steps) \
     SELECT l.id AS live_id, tb.id AS tombstone_id, tb.kind AS tombstone_kind, \
       n.id AS target_id, n.kind AS target_kind, n.owner_id AS target_owner, \
       p.id AS project_id, p.visibility, p.deleted_at IS NOT NULL AS pending_deletion, \
       a.steps, a.truncs, a.roles, a.rebuilt, \
       (NOT a.truncated AND a.reaches_top AND EXISTS (SELECT 1 FROM forge.namespaces x \
         WHERE x.full_path = a.rebuilt AND x.id = n.id)) AS confirmed \
     FROM agg a \
     LEFT JOIN live l ON true \
     LEFT JOIN tomb tb ON true \
     LEFT JOIN forge.namespaces n ON n.id = (SELECT id FROM target) \
     LEFT JOIN forge.projects p ON n.kind = 'project' AND p.id = n.owner_id"
);

/// `$1` the project id, `$2` the viewer (NULL = anonymous), `$3` the chain
/// cap: the project's row and the viewer's role chain from its namespace,
/// one row whether or not the project exists.
pub(crate) const PROJECT_ACCESS_SQL: &str = concat!(
    "WITH RECURSIVE ",
    chain_cte!("kind = 'project' AND owner_id = $1"),
    ", steps AS (SELECT ",
    chain_role_columns!(),
    " FROM chain c), \
       agg AS (SELECT array_agg(step ORDER BY step) AS steps, \
         array_agg(truncated ORDER BY step) AS truncs, \
         array_agg(role ORDER BY step) AS roles FROM steps) \
     SELECT p.id AS project_id, p.visibility, p.deleted_at IS NOT NULL AS pending_deletion, \
       a.steps, a.truncs, a.roles \
     FROM agg a LEFT JOIN forge.projects p ON p.id = $1"
);

/// The one row of [`RESOLVE_SQL`].
#[derive(Debug, sqlx::FromRow)]
struct ResolveRow {
    live_id: Option<i64>,
    tombstone_id: Option<i64>,
    tombstone_kind: Option<String>,
    target_id: Option<i64>,
    target_kind: Option<String>,
    target_owner: Option<i64>,
    #[sqlx(flatten)]
    project: ProjectFacts,
    #[sqlx(flatten)]
    chain: ChainRoles,
    rebuilt: Option<String>,
    confirmed: Option<bool>,
}

/// The project columns (NULL when the target is not a project or absent).
#[derive(Debug, sqlx::FromRow)]
struct ProjectFacts {
    project_id: Option<i64>,
    visibility: Option<String>,
    pending_deletion: Option<bool>,
}

/// The chain, one array element per step in step order (NULL arrays when
/// there is no target).
#[derive(Debug, sqlx::FromRow)]
struct ChainRoles {
    steps: Option<Vec<i32>>,
    truncs: Option<Vec<bool>>,
    roles: Option<Vec<Option<String>>>,
}

/// The one row of [`PROJECT_ACCESS_SQL`].
#[derive(Debug, sqlx::FromRow)]
struct ProjectAccessRow {
    #[sqlx(flatten)]
    project: ProjectFacts,
    #[sqlx(flatten)]
    chain: ChainRoles,
}

/// `FG-DB-0021` with its `reason`.
fn invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NamespaceDataInvalid).with_context("reason", reason)
}

/// A stored value for the log, bounded (the column is unchecked text once
/// its CHECK is gone).
fn bounded(raw: &str) -> String {
    raw.chars().take(32).collect()
}

impl ChainRoles {
    /// The viewer's roles over the chain ([`fold_chain`]: `FG-DB-0018` on a
    /// truncated chain or an unknown role). An empty chain has no role.
    fn fold(
        self,
        resource: &'static str,
        id: i64,
        viewer: Option<UserId>,
    ) -> Result<MemberRoles, ForgeError> {
        let steps = self.steps.unwrap_or_default();
        let truncs = self.truncs.unwrap_or_default();
        let roles = self.roles.unwrap_or_default();
        if steps.len() != truncs.len() || steps.len() != roles.len() {
            // array_agg over the same rows: only a broken statement gets here.
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "chain_arrays_disagree")
                .with_context("resource", resource)
                .with_context("id", id));
        }
        let rows = steps
            .into_iter()
            .zip(truncs)
            .zip(roles)
            .map(|((step, truncated), role)| (step, truncated, role));
        fold_chain(rows, resource, id, viewer)
    }
}

impl ProjectFacts {
    /// The project's access facts with the viewer's `role`; `None` when the
    /// projects row is absent.
    fn access(self, role: Option<forge_core::Role>) -> Result<Option<ProjectAccess>, ForgeError> {
        let Some(project_id) = self.project_id else {
            return Ok(None);
        };
        let raw = self.visibility.unwrap_or_default();
        let Some(visibility) = Visibility::parse(&raw) else {
            return Err(invalid("unknown_visibility")
                .with_context("project_id", project_id)
                .with_context("raw", bounded(&raw)));
        };
        Ok(Some(ProjectAccess {
            visibility,
            role,
            pending_deletion: self.pending_deletion.unwrap_or(false),
        }))
    }
}

/// The resolved target of `row` (its namespace, kind, owner, project facts
/// and the viewer's role).
fn resolved(
    namespace_id: i64,
    kind: Option<String>,
    owner: Option<i64>,
    project: ProjectFacts,
    chain: ChainRoles,
    viewer: Option<UserId>,
) -> Result<Resolved, ForgeError> {
    let raw = kind.unwrap_or_default();
    let Some(kind) = NamespaceKind::parse(&raw) else {
        return Err(invalid("unknown_namespace_kind")
            .with_context("namespace_id", namespace_id)
            .with_context("raw", bounded(&raw)));
    };
    // owner_id is NOT NULL; the row came with the namespace id.
    let owner = owner.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "namespace_row_incomplete")
            .with_context("namespace_id", namespace_id)
    })?;
    let roles = chain.fold("namespace", namespace_id, viewer)?;
    let target = match kind {
        NamespaceKind::User => ResolvedTarget::User(UserId::new(owner)),
        NamespaceKind::Group => ResolvedTarget::Group {
            id: GroupId::new(owner),
            viewer_role: roles.native,
        },
        NamespaceKind::Project => {
            let access = project.access(roles.native)?.ok_or_else(|| {
                invalid("namespace_without_project")
                    .with_context("namespace_id", namespace_id)
                    .with_context("project_id", owner)
            })?;
            ResolvedTarget::Project {
                id: ProjectId::new(owner),
                access,
            }
        }
    };
    Ok(Resolved {
        namespace_id: NamespaceId::new(namespace_id),
        target,
    })
}

/// Fold the one row of [`RESOLVE_SQL`] into the answer. Pure (no
/// statement): every branch was paid for by the same statement.
fn interpret(row: ResolveRow, viewer: Option<UserId>) -> Result<Resolution, ForgeError> {
    if let Some(live) = row.live_id {
        // Same snapshot: the target is the live row.
        if row.target_id != Some(live) {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "live_target_mismatch")
                .with_context("namespace_id", live));
        }
        let resolved = resolved(
            live,
            row.target_kind,
            row.target_owner,
            row.project,
            row.chain,
            viewer,
        )?;
        return Ok(Resolution::Live(resolved));
    }
    let Some(tombstone_id) = row.tombstone_id else {
        return Ok(Resolution::Absent);
    };
    let tombstone = TombstoneId::new(tombstone_id);
    let kind = row.tombstone_kind.unwrap_or_default();
    match kind.as_str() {
        "deleted" => Ok(Resolution::Deleted { tombstone }),
        "moved" => {
            // Only a target that is gone (namespace_id SET NULL by a purge)
            // makes a moved tombstone deleted (ADR-0015). A target that
            // still exists must resolve: a broken chain is FG-DB-0018 (the
            // fold, as for a live path) and a current path that cannot be
            // confirmed is FG-DB-0021 — never a silent Deleted.
            let Some(target) = row.target_id else {
                return Ok(Resolution::Deleted { tombstone });
            };
            let resolved = resolved(
                target,
                row.target_kind,
                row.target_owner,
                row.project,
                row.chain,
                viewer,
            )?;
            let (Some(true), Some(rebuilt)) = (row.confirmed, row.rebuilt) else {
                return Err(invalid("moved_path_unconfirmed")
                    .with_context("namespace_id", target)
                    .with_context("tombstone_id", tombstone_id));
            };
            let current = NamespacePath::parse(&rebuilt).map_err(|e| {
                let rule = e
                    .context()
                    .iter()
                    .find(|(k, _)| *k == "reason")
                    .map(|(_, v)| bounded(v))
                    .unwrap_or_default();
                invalid("stored_path_invalid")
                    .with_context("namespace_id", target)
                    .with_context("tombstone_id", tombstone_id)
                    .with_context("rule", rule)
            })?;
            Ok(Resolution::Moved {
                resolved,
                tombstone,
                current,
            })
        }
        other => Err(invalid("unknown_tombstone_kind")
            .with_context("tombstone_id", tombstone_id)
            .with_context("raw", bounded(other))),
    }
}

/// What is at `path` and what `viewer` (`None` = anonymous) may need to be
/// authorized there: one [`RESOLVE_SQL`] round trip on the read pool,
/// whatever the answer (see the module docs and
/// [`forge_core::NamespaceResolver`]). Errors: `FG-DB-0001` / `FG-DB-0002`
/// (store), `FG-DB-0018` (role chain), `FG-DB-0021` (rows this build
/// cannot read).
pub async fn resolve(
    db: &Db,
    path: &LookupPath,
    viewer: Option<UserId>,
) -> Result<Resolution, ForgeError> {
    let full_path = path.full_path();
    let row: ResolveRow = db
        .within("namespaces.resolve", async {
            sqlx::query_as(RESOLVE_SQL)
                .bind(full_path.as_str())
                .bind(viewer.map(UserId::get))
                .bind(CAP)
                .fetch_one(db.pool().executor())
                .await
                .map_err(|e| read_query_error("namespaces.resolve", e))
        })
        .await?;
    interpret(row, viewer)
}

/// A project's access facts for `viewer` (`None` = anonymous), `None` when
/// the project does not exist: one [`PROJECT_ACCESS_SQL`] round trip on the
/// read pool whether it exists or not. The role is the role loaders'
/// `native` (`roles::load_project_roles`). Errors as [`resolve`];
/// `FG-DB-0021 project_without_namespace` for a project row whose
/// namespace is missing.
pub async fn project_access(
    db: &Db,
    project: ProjectId,
    viewer: Option<UserId>,
) -> Result<Option<ProjectAccess>, ForgeError> {
    let row: ProjectAccessRow = db
        .within("projects.access", async {
            sqlx::query_as(PROJECT_ACCESS_SQL)
                .bind(project.get())
                .bind(viewer.map(UserId::get))
                .bind(CAP)
                .fetch_one(db.pool().executor())
                .await
                .map_err(|e| read_query_error("projects.access", e))
        })
        .await?;
    if row.project.project_id.is_none() {
        return Ok(None);
    }
    if row.chain.steps.as_ref().is_none_or(Vec::is_empty) {
        return Err(invalid("project_without_namespace").with_context("project_id", project.get()));
    }
    let roles = row.chain.fold("project", project.get(), viewer)?;
    row.project.access(roles.native)
}

/// The dispatcher's resolver (`forge serve` injects the `Db`): exactly
/// [`resolve`].
impl forge_core::NamespaceResolver for Db {
    fn resolve<'a>(
        &'a self,
        path: &'a LookupPath,
        viewer: Option<UserId>,
    ) -> LookupFuture<'a, Resolution> {
        Box::pin(resolve(self, path, viewer))
    }
}

/// Project access by id (`GET /api/v1/projects/{id}`): exactly
/// [`project_access`].
impl forge_core::ProjectAccessLoader for Db {
    fn project_access<'a>(
        &'a self,
        project: ProjectId,
        viewer: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectAccess>> {
        Box::pin(project_access(self, project, viewer))
    }
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
