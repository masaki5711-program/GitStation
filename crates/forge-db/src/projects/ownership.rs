//! Projects without an active Owner (D4 MR4, ADR-0019 Amendment D4 MR4):
//! the project half of the disable guard (`users::disable`) and of what
//! `forge doctor` `users.administration` reports.
//!
//! A project's Owners are, as `roles::load_project_roles` computes them,
//! its direct Owners, the direct Owners of its ancestor groups and the owner
//! of the personal namespace it is directly in (not a bot) — active users
//! only. A project "has no active Owner" when none of them is left. Projects
//! pending deletion (`deleted_at` set, D5) are not counted: nobody manages
//! their members until they are restored.
//!
//! Both queries read without locks. The guard's query runs inside the
//! disable's change after the user row is locked `FOR UPDATE`; project
//! member changes lock the project row and then the remaining Owners' user
//! rows `FOR SHARE` (`projects::members`), so the user row serializes the
//! two and project rows are never locked here (ADR-0019 §11 order).

use forge_core::ids::{ProjectId, UserId};
use forge_core::namespace_path::MAX_DEPTH;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::{query_error, read_query_error, Db};

/// The cap on namespace chains, as the SQL parameter (as in `roles`).
const CAP: i32 = {
    assert!(MAX_DEPTH <= i32::MAX as usize);
    MAX_DEPTH as i32
};

/// How many ownerless project ids the doctor finding carries.
const SHOWN_PROJECT_IDS: i64 = 10;

/// `$1` = the projects to look at (`candidates (project_id)`, defined by
/// the caller's SQL before this fragment), `$2` = the user who no longer
/// counts (0 = nobody), `$3` = the cap. Selects the candidates that are not
/// pending deletion and have no active Owner other than `$2`.
macro_rules! without_owner_sql {
    ($candidates:literal, $tail:literal) => {
        concat!(
            "WITH RECURSIVE ",
            $candidates,
            ", up (project_id, parent_id, kind, owner_id, step) AS ( \
                 SELECT c.project_id, n.parent_id, n.kind, n.owner_id, 1 \
                 FROM candidates c JOIN forge.namespaces n \
                   ON n.kind = 'project' AND n.owner_id = c.project_id \
               UNION ALL \
                 SELECT u.project_id, n.parent_id, n.kind, n.owner_id, u.step + 1 \
                 FROM forge.namespaces n JOIN up u ON n.id = u.parent_id WHERE u.step < $3), \
             ownerless AS ( \
               SELECT c.project_id AS id FROM candidates c \
               JOIN forge.projects p ON p.id = c.project_id \
               WHERE p.deleted_at IS NULL \
               AND NOT EXISTS (SELECT 1 FROM forge.project_members m \
                   JOIN forge.users u ON u.id = m.user_id \
                   WHERE m.project_id = c.project_id AND m.role = 'owner' \
                   AND m.user_id <> $2 AND u.state = 'active') \
               AND NOT EXISTS (SELECT 1 FROM up \
                   JOIN forge.group_members m ON up.kind = 'group' \
                     AND m.group_id = up.owner_id AND m.role = 'owner' \
                   JOIN forge.users u ON u.id = m.user_id \
                   WHERE up.project_id = c.project_id AND m.user_id <> $2 \
                   AND u.state = 'active') \
               AND NOT EXISTS (SELECT 1 FROM up \
                   JOIN forge.users u ON up.kind = 'user' AND up.step = 2 \
                     AND u.id = up.owner_id \
                   WHERE up.project_id = c.project_id AND u.id <> $2 \
                   AND u.state = 'active' AND u.kind <> 'bot')) ",
            $tail
        )
    };
}

/// The projects where `$1` holds Owner (direct, through a group it directly
/// owns and the groups below it, or through its personal namespace) and
/// that would have no other active Owner.
const AFTER_DISABLE: &str = without_owner_sql!(
    "roots (id) AS ( \
         SELECT n.id FROM forge.group_members m \
         JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = m.group_id \
         WHERE m.user_id = $1 AND m.role = 'owner' \
       UNION \
         SELECT n.id FROM forge.namespaces n WHERE n.kind = 'user' AND n.owner_id = $1), \
     down (id, kind, owner_id, step) AS ( \
         SELECT n.id, n.kind, n.owner_id, 1 FROM forge.namespaces n \
         WHERE n.id IN (SELECT id FROM roots) \
       UNION ALL \
         SELECT n.id, n.kind, n.owner_id, d.step + 1 \
         FROM forge.namespaces n JOIN down d ON n.parent_id = d.id WHERE d.step < $3), \
     candidates (project_id) AS ( \
         SELECT owner_id FROM down WHERE kind = 'project' \
       UNION \
         SELECT project_id FROM forge.project_members WHERE user_id = $1 AND role = 'owner')",
    "SELECT id FROM ownerless ORDER BY id"
);

/// Every project without an active Owner: the count and the first ids.
const EVERY: &str = without_owner_sql!(
    "candidates (project_id) AS (SELECT id FROM forge.projects)",
    "SELECT (SELECT pg_catalog.count(*) FROM ownerless), \
     ARRAY(SELECT id FROM ownerless ORDER BY id LIMIT $1)"
);

/// The projects disabling `user` would leave without an active Owner (the
/// disable guard, ADR-0019 Amendment D4 MR4), by id. Runs in the disable's
/// change, after the user row lock.
pub(crate) async fn after_disable(
    conn: &mut PgConnection,
    user: UserId,
) -> Result<Vec<i64>, ForgeError> {
    sqlx::query_scalar(AFTER_DISABLE)
        .bind(user.get())
        .bind(user.get())
        .bind(CAP)
        .fetch_all(conn)
        .await
        .map_err(|e| query_error("users.disable.project_owner_count", e))
}

/// The projects without an active Owner, as `forge doctor` sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ownerless {
    /// How many projects (not pending deletion) have no active Owner.
    pub count: i64,
    /// The first of them (at most 10), by id.
    pub project_ids: Vec<ProjectId>,
}

impl Ownerless {
    /// `FG-AUTH-0010` (warn) `reason=ownerless_project` when a project has
    /// no active Owner; `ownerless_projects` and `project_ids`, never names.
    pub fn check(&self) -> Result<(), ForgeError> {
        if self.count == 0 {
            return Ok(());
        }
        let ids: Vec<String> = self
            .project_ids
            .iter()
            .map(|p| p.get().to_string())
            .collect();
        Err(ForgeError::new(ErrorCode::AdministrationOrphaned)
            .with_context("reason", "ownerless_project")
            .with_context("ownerless_projects", self.count)
            .with_context("project_ids", ids.join(",")))
    }
}

/// Read the projects without an active Owner on the read pool, in one
/// statement; `None` before the projects migration (the doctor skips it).
/// For `forge doctor` `users.administration` (wired there at merge time).
pub async fn ownerless(db: &Db) -> Result<Option<Ownerless>, ForgeError> {
    const QUERY: &str = "projects.ownerless";
    db.within(QUERY, async {
        let mut conn = db.acquire().await?;
        let migrated: bool = sqlx::query_scalar(
            "SELECT pg_catalog.to_regclass('forge.project_members') IS NOT NULL",
        )
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        if !migrated {
            return Ok(None);
        }
        // $2 = 0: no user id is 0 (identity from 1), so every Owner counts.
        let (count, shown): (i64, Vec<i64>) = sqlx::query_as(EVERY)
            .bind(SHOWN_PROJECT_IDS)
            .bind(0_i64)
            .bind(CAP)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        Ok(Some(Ownerless {
            count,
            project_ids: shown.into_iter().map(ProjectId::new).collect(),
        }))
    })
    .await
}
