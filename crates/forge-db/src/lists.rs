//! Keyset page scans for the API lists (M1 phase 8b MR7b): forge-db's
//! [`forge_core::ProjectPageLoader`] and [`forge_core::GroupPageLoader`],
//! implemented on [`Db`].
//!
//! **One statement per page, whatever the rows.** [`project_page`] runs
//! [`PROJECT_PAGE_SQL`] and [`group_page`] runs [`GROUP_PAGE_SQL`] exactly
//! once on the read pool under [`Db::within`]. The statement returns every
//! project (group) with an id above `$1`, in id order, at most `$4` — the
//! visible and the invisible alike. **Nothing here filters by visibility**
//! (plan option L1): each row carries the viewer's raw authorization inputs
//! (visibility, pending deletion, the effective role over the parent
//! chain) and forge-web decides it with `authorize()`.
//!
//! Each row is the single-object loader's row: the role comes from the role
//! loaders' chain (`roles::chain_cte!` shape, `roles::chain_role_columns!`,
//! `roles::fold_chain`), anchored at every scanned namespace at once, and a
//! project row is read by [`crate::resolve::interpret_project`] — the same
//! columns as `PROJECT_ACCESS_SQL`, so a listed project and
//! `GET /api/v1/projects/{id}` cannot disagree.

use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    GroupId, GroupListing, NamespaceId, NamespacePath, ProjectId, ProjectRecord, UserId,
};
use forge_errors::{ErrorCode, ForgeError};

use crate::resolve::{interpret_project, invalid, parse_rule, ChainRoles, ProjectAccessRow};
use crate::roles::{chain_role_columns, CAP};
use crate::{read_query_error, Db};

/// The largest page a caller may scan in one statement (forge-web asks for
/// its scan budget plus one).
pub const MAX_PAGE_ROWS: u32 = 10_000;

/// The scan of one page of `$table` (`forge.projects` / `forge.groups`)
/// whose namespaces are of `$kind`: `page` (the ids above `$1`, in order,
/// at most `$4`), the parent chain of every scanned namespace capped at
/// `$3` steps (`anchor` = the scanned id), the viewer's (`$2`) role at
/// each step, and `agg` (one row per anchor: steps, roles, the path rebuilt
/// from the segments, top first). The caller's `SELECT` joins the rest.
macro_rules! page_scan {
    ($table:literal, $kind:literal) => {
        concat!(
            "WITH RECURSIVE \
               page AS (SELECT id FROM ",
            $table,
            " WHERE id > $1 ORDER BY id LIMIT $4), \
               chain (anchor, id, parent_id, kind, owner_id, segment, step, anchor_kind) AS ( \
                 SELECT pg.id, n.id, n.parent_id, n.kind, n.owner_id, n.segment, 1, n.kind \
                 FROM page pg JOIN forge.namespaces n ON n.kind = '",
            $kind,
            "' AND n.owner_id = pg.id \
                 UNION ALL \
                 SELECT c.anchor, n.id, n.parent_id, n.kind, n.owner_id, n.segment, c.step + 1, \
                   c.anchor_kind \
                 FROM forge.namespaces n JOIN chain c ON n.id = c.parent_id \
                 WHERE c.step < $3), \
               steps AS (SELECT c.anchor, c.segment, c.parent_id AS up, ",
            chain_role_columns!(),
            " FROM chain c), \
               agg AS (SELECT anchor, array_agg(step ORDER BY step) AS steps, \
                 array_agg(truncated ORDER BY step) AS truncs, \
                 array_agg(role ORDER BY step) AS roles, \
                 string_agg(segment, '/' ORDER BY step DESC) AS rebuilt, \
                 coalesce(bool_or(truncated), false) AS truncated, \
                 coalesce(bool_or(up IS NULL), false) AS reaches_top \
                 FROM steps GROUP BY anchor) "
        )
    };
}

/// `$1` the last id of the previous page (0 = from the start), `$2` the
/// viewer, `$3` the chain cap, `$4` the most rows: the columns of
/// `PROJECT_ACCESS_SQL`, one row per scanned project, in id order.
pub(crate) const PROJECT_PAGE_SQL: &str = concat!(
    page_scan!("forge.projects", "project"),
    "SELECT p.id AS project_id, p.visibility, p.deleted_at IS NOT NULL AS pending_deletion, \
       a.steps, a.truncs, a.roles, \
       n.id AS namespace_id, n.full_path, \
       (NOT a.truncated AND a.reaches_top AND a.rebuilt = n.full_path) AS confirmed, \
       p.display_name, p.description, r.kind AS repository_kind, \
       r.id AS repository_id, r.state AS repository_state, \
       to_char(p.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at \
     FROM page pg JOIN forge.projects p ON p.id = pg.id \
     LEFT JOIN agg a ON a.anchor = p.id \
     LEFT JOIN forge.namespaces n ON n.kind = 'project' AND n.owner_id = p.id \
     LEFT JOIN forge.repositories r ON r.project_id = p.id \
     ORDER BY p.id"
);

/// As [`PROJECT_PAGE_SQL`], for groups: the group's row, its namespace and
/// the viewer's chain.
pub(crate) const GROUP_PAGE_SQL: &str = concat!(
    page_scan!("forge.groups", "group"),
    "SELECT g.id AS group_id, a.steps, a.truncs, a.roles, \
       n.id AS namespace_id, n.full_path, \
       (NOT a.truncated AND a.reaches_top AND a.rebuilt = n.full_path) AS confirmed, \
       g.display_name, g.description, \
       to_char(g.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at \
     FROM page pg JOIN forge.groups g ON g.id = pg.id \
     LEFT JOIN agg a ON a.anchor = g.id \
     LEFT JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = g.id \
     ORDER BY g.id"
);

/// One row of [`GROUP_PAGE_SQL`].
#[derive(Debug, sqlx::FromRow)]
struct GroupPageRow {
    group_id: i64,
    #[sqlx(flatten)]
    chain: ChainRoles,
    namespace_id: Option<i64>,
    full_path: Option<String>,
    confirmed: Option<bool>,
    display_name: String,
    description: String,
    created_at: String,
}

/// `$1`: the position after which the page starts (ids are positive).
fn after_param(after: Option<i64>) -> i64 {
    after.unwrap_or(0)
}

/// `max_rows` as `$4`: 1 ..= [`MAX_PAGE_ROWS`], else a programming error
/// (`FG-SYS-0001 reason=page_size_out_of_range`).
fn rows_param(max_rows: u32) -> Result<i64, ForgeError> {
    if max_rows == 0 || max_rows > MAX_PAGE_ROWS {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "page_size_out_of_range")
            .with_context("max_rows", max_rows));
    }
    Ok(i64::from(max_rows))
}

/// The projects after `after`, in id order, at most `max_rows`, with
/// `viewer`'s access facts — every project, visible or not: one
/// [`PROJECT_PAGE_SQL`] round trip on the read pool. Each row is read as
/// `resolve::load_project` reads its row (the same `FG-DB-0021` reasons).
pub async fn project_page(
    db: &Db,
    after: Option<ProjectId>,
    viewer: UserId,
    max_rows: u32,
) -> Result<Vec<ProjectRecord>, ForgeError> {
    let limit = rows_param(max_rows)?;
    let rows: Vec<ProjectAccessRow> = db
        .within("projects.page", async {
            sqlx::query_as(PROJECT_PAGE_SQL)
                .bind(after_param(after.map(ProjectId::get)))
                .bind(viewer.get())
                .bind(CAP)
                .bind(limit)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error("projects.page", e))
        })
        .await?;
    let mut records = Vec::with_capacity(rows.len());
    for row in rows {
        // The page joins projects: every row has its id.
        let id = row.project_id().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "page_row_without_id")
        })?;
        let project = ProjectId::new(id);
        match interpret_project(row, project, Some(viewer))? {
            Some(record) => records.push(record),
            None => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "page_row_without_project")
                    .with_context("project_id", id))
            }
        }
    }
    Ok(records)
}

/// As [`project_page`], for groups: one [`GROUP_PAGE_SQL`] round trip.
/// `FG-DB-0021` `group_without_namespace` / `group_path_unconfirmed` /
/// `stored_path_invalid`; `FG-DB-0018` for a broken chain.
pub async fn group_page(
    db: &Db,
    after: Option<GroupId>,
    viewer: UserId,
    max_rows: u32,
) -> Result<Vec<GroupListing>, ForgeError> {
    let limit = rows_param(max_rows)?;
    let rows: Vec<GroupPageRow> = db
        .within("groups.page", async {
            sqlx::query_as(GROUP_PAGE_SQL)
                .bind(after_param(after.map(GroupId::get)))
                .bind(viewer.get())
                .bind(CAP)
                .bind(limit)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error("groups.page", e))
        })
        .await?;
    rows.into_iter()
        .map(|row| interpret_group(row, viewer))
        .collect()
}

/// Read one [`GROUP_PAGE_SQL`] row. Pure (no statement).
fn interpret_group(row: GroupPageRow, viewer: UserId) -> Result<GroupListing, ForgeError> {
    let id = row.group_id;
    let (Some(namespace_id), Some(full_path)) = (row.namespace_id, row.full_path) else {
        return Err(invalid("group_without_namespace").with_context("group_id", id));
    };
    if row.chain.is_empty() {
        return Err(invalid("group_without_namespace").with_context("group_id", id));
    }
    let roles = row.chain.fold("group", id, Some(viewer))?;
    if row.confirmed != Some(true) {
        return Err(invalid("group_path_unconfirmed")
            .with_context("group_id", id)
            .with_context("namespace_id", namespace_id));
    }
    let path = NamespacePath::parse(&full_path).map_err(|e| {
        invalid("stored_path_invalid")
            .with_context("group_id", id)
            .with_context("namespace_id", namespace_id)
            .with_context("rule", parse_rule(&e))
    })?;
    Ok(GroupListing {
        id: GroupId::new(id),
        viewer_role: roles.native,
        namespace_id: NamespaceId::new(namespace_id),
        path,
        display_name: row.display_name,
        description: row.description,
        created_at: row.created_at,
    })
}

/// Project pages for `GET /api/v1/projects`: exactly [`project_page`].
impl forge_core::ProjectPageLoader for Db {
    fn project_page<'a>(
        &'a self,
        after: Option<ProjectId>,
        viewer: UserId,
        max_rows: u32,
    ) -> LookupFuture<'a, Vec<ProjectRecord>> {
        Box::pin(project_page(self, after, viewer, max_rows))
    }
}

/// Group pages for `GET /api/v1/groups`: exactly [`group_page`].
impl forge_core::GroupPageLoader for Db {
    fn group_page<'a>(
        &'a self,
        after: Option<GroupId>,
        viewer: UserId,
        max_rows: u32,
    ) -> LookupFuture<'a, Vec<GroupListing>> {
        Box::pin(group_page(self, after, viewer, max_rows))
    }
}

#[cfg(test)]
#[path = "lists_tests.rs"]
mod tests;
