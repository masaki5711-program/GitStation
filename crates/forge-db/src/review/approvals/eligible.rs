//! Who could approve (ADR-0038 §3.3, step C): the facts of people named by
//! an approver list (users, the direct members of groups, the people a
//! selector finds) as they are now — active, kind, role on the project,
//! names. For display and for `escalate: parent` ("is anyone there?");
//! never a merge decision, which counts approvals only.

use std::collections::BTreeMap;

use forge_core::ids::{GroupId, ProjectId, UserId};
use forge_core::{Role, UserKind};
use forge_errors::{ErrorCode, ForgeError};

use crate::{read_query_error, Db};

/// The most people one read answers (a page of candidates).
pub const MAX_PEOPLE: usize = 1000;

/// One person as an approver candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonFacts {
    pub user: UserId,
    pub username: String,
    pub display_name: String,
    pub active: bool,
    pub kind: UserKind,
    /// The native role on the project now (`None`: none).
    pub role: Option<Role>,
}

/// The facts of `users` on `project` (read pool, two statements; at most
/// [`MAX_PEOPLE`]): unknown ids are absent from the answer.
pub async fn people_facts(
    db: &Db,
    project: ProjectId,
    users: &[UserId],
) -> Result<BTreeMap<UserId, PersonFacts>, ForgeError> {
    const QUERY: &str = "review.people_facts";
    if users.is_empty() {
        return Ok(BTreeMap::new());
    }
    if users.len() > MAX_PEOPLE {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "too_many_people")
            .with_context("count", users.len()));
    }
    let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
    db.within(QUERY, async {
        let mut conn = db
            .pool()
            .executor()
            .acquire()
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        let accounts: Vec<(i64, String, String, bool, String)> = sqlx::query_as(
            "SELECT id, username, display_name, state = 'active', kind FROM forge.users \
             WHERE id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let found: Vec<UserId> = accounts.iter().map(|a| UserId::new(a.0)).collect();
        let mut roles = crate::roles::project_roles_on(&mut conn, project, &found).await?;
        let mut out = BTreeMap::new();
        for (id, username, display_name, active, kind) in accounts {
            let user = UserId::new(id);
            let kind = UserKind::parse(&kind).ok_or_else(|| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "unknown_user_kind")
                    .with_context("user_id", id)
            })?;
            out.insert(
                user,
                PersonFacts {
                    user,
                    username,
                    display_name,
                    active,
                    kind,
                    role: roles.remove(&user).flatten(),
                },
            );
        }
        Ok(out)
    })
    .await
}

/// The direct members of `groups` (read pool), at most `limit` rows.
pub async fn group_members(
    db: &Db,
    groups: &[GroupId],
    limit: usize,
) -> Result<Vec<(GroupId, UserId)>, ForgeError> {
    const QUERY: &str = "review.group_members";
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = groups.iter().map(|g| g.get()).collect();
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows: Vec<(i64, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT group_id, user_id FROM forge.group_members WHERE group_id = ANY($1) \
                 ORDER BY group_id, user_id LIMIT $2",
            )
            .bind(&ids)
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(g, u)| (GroupId::new(g), UserId::new(u)))
        .collect())
}
