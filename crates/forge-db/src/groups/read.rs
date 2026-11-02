//! One group by id for a viewer (M1 phase 8b MR7a): the group routes'
//! loader. One statement on the read pool whether the group exists or not,
//! as `resolve::load_project` (the viewer's role from the role loaders'
//! chain, the details from the same row).

use forge_core::ids::{GroupId, NamespaceId, UserId};
use forge_core::{GroupRecord, NamespacePath};
use forge_errors::{ErrorCode, ForgeError};

use crate::roles::{chain_cte, chain_role_columns, fold_chain, CAP};
use crate::{read_query_error, Db};

/// `$1` the group id, `$2` the viewer (NULL = anonymous), `$3` the chain
/// cap. One row: the chain's roles and rebuilt path (aggregates), the
/// group row, its namespace and the parent namespace's owner (the parent
/// group), all `LEFT JOIN`ed.
const GROUP_SQL: &str = concat!(
    "WITH RECURSIVE ",
    chain_cte!("kind = 'group' AND owner_id = $1"),
    ", steps AS (SELECT c.segment, c.parent_id AS up, ",
    chain_role_columns!(),
    " FROM chain c), \
       agg AS (SELECT array_agg(step ORDER BY step) AS steps, \
         array_agg(truncated ORDER BY step) AS truncs, \
         array_agg(role ORDER BY step) AS roles, \
         string_agg(segment, '/' ORDER BY step DESC) AS rebuilt, \
         coalesce(bool_or(truncated), false) AS truncated, \
         coalesce(bool_or(up IS NULL), false) AS reaches_top FROM steps) \
     SELECT g.id AS group_id, a.steps, a.truncs, a.roles, \
       n.id AS namespace_id, n.full_path, pn.kind AS parent_kind, pn.owner_id AS parent_owner, \
       (NOT a.truncated AND a.reaches_top AND a.rebuilt = n.full_path) AS confirmed, \
       g.display_name, g.description, \
       to_char(g.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at \
     FROM agg a LEFT JOIN forge.groups g ON g.id = $1 \
     LEFT JOIN forge.namespaces n ON g.id IS NOT NULL AND n.kind = 'group' AND n.owner_id = $1 \
     LEFT JOIN forge.namespaces pn ON pn.id = n.parent_id"
);

#[derive(Debug, sqlx::FromRow)]
struct GroupRow {
    group_id: Option<i64>,
    steps: Option<Vec<i32>>,
    truncs: Option<Vec<bool>>,
    roles: Option<Vec<Option<String>>>,
    namespace_id: Option<i64>,
    full_path: Option<String>,
    parent_kind: Option<String>,
    parent_owner: Option<i64>,
    confirmed: Option<bool>,
    display_name: Option<String>,
    description: Option<String>,
    created_at: Option<String>,
}

/// `FG-DB-0021` (the resolver's code for stored namespace data it cannot
/// read) with its `reason`.
fn invalid(reason: &'static str, group: i64) -> ForgeError {
    ForgeError::new(ErrorCode::NamespaceDataInvalid)
        .with_context("reason", reason)
        .with_context("group_id", group)
}

/// One group for `viewer`, `None` when it does not exist. The role is the
/// role loaders' `native`. `FG-DB-0021` for a group without its namespace
/// (`group_without_namespace`), a stored path its segments do not confirm
/// (`group_path_unconfirmed`) or that does not parse
/// (`stored_path_invalid`), or a parent that is not a group
/// (`group_parent_not_group`); `FG-DB-0018` for the chain (as the role
/// loaders).
pub async fn load(
    db: &Db,
    group: GroupId,
    viewer: Option<UserId>,
) -> Result<Option<GroupRecord>, ForgeError> {
    let row: GroupRow = db
        .within("groups.load", async {
            sqlx::query_as(GROUP_SQL)
                .bind(group.get())
                .bind(viewer.map(UserId::get))
                .bind(CAP)
                .fetch_one(db.pool().executor())
                .await
                .map_err(|e| read_query_error("groups.load", e))
        })
        .await?;
    interpret(row, group, viewer)
}

fn interpret(
    row: GroupRow,
    group: GroupId,
    viewer: Option<UserId>,
) -> Result<Option<GroupRecord>, ForgeError> {
    let id = group.get();
    if row.group_id.is_none() {
        return Ok(None);
    }
    let (Some(namespace_id), Some(full_path)) = (row.namespace_id, row.full_path) else {
        return Err(invalid("group_without_namespace", id));
    };
    let (Some(steps), Some(truncs), Some(roles)) = (row.steps, row.truncs, row.roles) else {
        return Err(invalid("group_without_namespace", id));
    };
    if steps.len() != truncs.len() || steps.len() != roles.len() || steps.is_empty() {
        return Err(invalid("group_without_namespace", id));
    }
    let chain = steps
        .into_iter()
        .zip(truncs)
        .zip(roles)
        .map(|((s, t), r)| (s, t, r));
    let roles = fold_chain(chain, "group", id, viewer)?;
    if row.confirmed != Some(true) {
        return Err(invalid("group_path_unconfirmed", id));
    }
    let path = NamespacePath::parse(&full_path)
        .map_err(|e| invalid("stored_path_invalid", id).with_source(e))?;
    let parent_id = match (row.parent_kind.as_deref(), row.parent_owner) {
        (None, _) => None,
        (Some("group"), Some(owner)) => Some(GroupId::new(owner)),
        (Some(_), _) => return Err(invalid("group_parent_not_group", id)),
    };
    let (Some(display_name), Some(description), Some(created_at)) =
        (row.display_name, row.description, row.created_at)
    else {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "group_row_incomplete")
            .with_context("group_id", id));
    };
    Ok(Some(GroupRecord {
        id: group,
        role: roles.native,
        namespace_id: NamespaceId::new(namespace_id),
        parent_id,
        path,
        display_name,
        description,
        created_at,
    }))
}
