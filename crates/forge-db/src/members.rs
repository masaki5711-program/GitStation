//! The direct members of a project or a group as the API lists them, and
//! one member's direct role for a route loader (M1 phase 8b MR8). Reads on
//! the read pool, one bounded statement each; the decision that binds a
//! member change reads the role again under the lock
//! (`projects::change_member_decided`, `groups::change_member_decided`).
//!
//! A page is keyset-ordered by user id (`after` = the last user id of the
//! previous page): a member list shows nothing the caller may not see
//! (`authorize()` allowed `MemberList` on the whole project or group), so a
//! plain id cursor reveals nothing (unlike the project list's AEAD cursor).

use forge_core::ids::{GroupId, ProjectId, UserId};
use forge_core::Role;
use forge_errors::{ErrorCode, ForgeError};

use crate::{read_query_error, Db};

/// The most members one page holds.
pub const MAX_PAGE: usize = 100;

/// One direct member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRow {
    pub user_id: UserId,
    pub username: String,
    pub display_name: String,
    /// The direct role (inherited roles are not listed).
    pub role: Role,
    /// The account is active (a disabled member holds no role meanwhile).
    pub active: bool,
    /// When the membership was created (UTC, RFC 3339).
    pub created_at: String,
}

/// Where the members are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberScope {
    Project(ProjectId),
    Group(GroupId),
}

impl MemberScope {
    /// The audit resource type and id (for the log).
    fn resource(self) -> (&'static str, i64) {
        match self {
            MemberScope::Project(p) => ("project", p.get()),
            MemberScope::Group(g) => ("group", g.get()),
        }
    }
}

const PROJECT_PAGE: &str = "\
    SELECT m.user_id, u.username, u.display_name, m.role, u.state = 'active', \
      to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
    FROM forge.project_members m JOIN forge.users u ON u.id = m.user_id \
    WHERE m.project_id = $1 AND m.user_id > $2 ORDER BY m.user_id LIMIT $3";
const GROUP_PAGE: &str = "\
    SELECT m.user_id, u.username, u.display_name, m.role, u.state = 'active', \
      to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
    FROM forge.group_members m JOIN forge.users u ON u.id = m.user_id \
    WHERE m.group_id = $1 AND m.user_id > $2 ORDER BY m.user_id LIMIT $3";

const PROJECT_ONE: &str = "\
    SELECT m.user_id, u.username, u.display_name, m.role, u.state = 'active', \
      to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
    FROM forge.project_members m JOIN forge.users u ON u.id = m.user_id \
    WHERE m.project_id = $1 AND m.user_id = $2";
const GROUP_ONE: &str = "\
    SELECT m.user_id, u.username, u.display_name, m.role, u.state = 'active', \
      to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
    FROM forge.group_members m JOIN forge.users u ON u.id = m.user_id \
    WHERE m.group_id = $1 AND m.user_id = $2";

type Row = (i64, String, String, String, bool, String);

fn member_row(scope: MemberScope, row: Row) -> Result<MemberRow, ForgeError> {
    let (user, username, display_name, role, active, created_at) = row;
    Ok(MemberRow {
        user_id: UserId::new(user),
        username,
        display_name,
        role: Role::parse(&role).ok_or_else(|| unknown_role(scope, user, &role))?,
        active,
        created_at,
    })
}

/// `FG-DB-0018 unknown_role` for a stored role outside the five (bounded).
fn unknown_role(scope: MemberScope, user: i64, raw: &str) -> ForgeError {
    let (resource, id) = scope.resource();
    ForgeError::new(ErrorCode::MembershipDataInvalid)
        .with_context("reason", "unknown_role")
        .with_context("resource", resource)
        .with_context("id", id)
        .with_context("user", user)
        .with_context("raw", raw.chars().take(32).collect::<String>())
}

/// One page of the direct members of `scope` after user id `after`
/// (`None` = from the start), at most `limit` (1 ..= [`MAX_PAGE`]; other
/// values are clamped). An unknown project or group has no members (the
/// caller has already loaded it).
pub async fn page(
    db: &Db,
    scope: MemberScope,
    after: Option<UserId>,
    limit: usize,
) -> Result<Vec<MemberRow>, ForgeError> {
    const QUERY: &str = "members.page";
    let (sql, id) = match scope {
        MemberScope::Project(p) => (PROJECT_PAGE, p.get()),
        MemberScope::Group(g) => (GROUP_PAGE, g.get()),
    };
    let limit = i64::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(1);
    let rows: Vec<Row> = db
        .within(QUERY, async {
            sqlx::query_as(sql)
                .bind(id)
                .bind(after.map_or(0, UserId::get))
                .bind(limit)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(|row| member_row(scope, row)).collect()
}

/// One direct member of `scope` (`None` = not a direct member): the row a
/// write route answers with after its commit.
pub async fn one(
    db: &Db,
    scope: MemberScope,
    member: UserId,
) -> Result<Option<MemberRow>, ForgeError> {
    const QUERY: &str = "members.one";
    let (sql, id) = match scope {
        MemberScope::Project(p) => (PROJECT_ONE, p.get()),
        MemberScope::Group(g) => (GROUP_ONE, g.get()),
    };
    let row: Option<Row> = db
        .within(QUERY, async {
            sqlx::query_as(sql)
                .bind(id)
                .bind(member.get())
                .fetch_optional(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(|row| member_row(scope, row)).transpose()
}

/// `member`'s direct role in `scope` (`None` = not a direct member), for a
/// route loader.
pub async fn direct_role(
    db: &Db,
    scope: MemberScope,
    member: UserId,
) -> Result<Option<Role>, ForgeError> {
    const QUERY: &str = "members.direct_role";
    let (sql, id) = match scope {
        MemberScope::Project(p) => (
            "SELECT role FROM forge.project_members WHERE project_id = $1 AND user_id = $2",
            p.get(),
        ),
        MemberScope::Group(g) => (
            "SELECT role FROM forge.group_members WHERE group_id = $1 AND user_id = $2",
            g.get(),
        ),
    };
    let role: Option<String> = db
        .within(QUERY, async {
            sqlx::query_scalar(sql)
                .bind(id)
                .bind(member.get())
                .fetch_optional(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    role.map(|raw| Role::parse(&raw).ok_or_else(|| unknown_role(scope, member.get(), &raw)))
        .transpose()
}
