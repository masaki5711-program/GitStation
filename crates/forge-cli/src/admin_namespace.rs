//! `forge admin project move`, `forge admin group move` and `forge admin
//! user rename` (step D5 MR5, ADR-0015 Amendment D5; runbook
//! namespace-redirects). Mutating operator commands: the host operator is
//! the trust root (actor `system`, `actor_label` naming the CLI and the OS
//! account), outside `authorize()` like the other operator commands
//! (ADR-0019 §8), and in production an approved runbook step. The same
//! forge-db move as the API (every subtree rule, the Owner invariant, the
//! tombstones, the audit rows and the move event) through
//! `forge_hosting::Hosting::operator_*`, **exempt from the rate limit of 10
//! moves per 24 hours** (step D5 plan: only the operator CLI).
//!
//! Prints one tab-separated line: kind, id, old path, new path, tombstones
//! written, projects moved.

use forge_core::config::DatabaseConfig;
use forge_core::{GroupId, NamespaceId, ProjectId, RequestId};
use forge_db::namespaces::{Destination, MoveOutcome};
use forge_db::users;
use forge_db::Db;
use forge_errors::ForgeError;
use forge_hosting::Hosting;

use crate::admin_token::{context, with_db};

/// Where a project or group goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveArgs {
    /// The new parent's namespace id (`None` = the top level; groups only).
    pub namespace: Option<i64>,
    /// The new last segment.
    pub path: String,
    pub reclaim: bool,
}

fn destination(args: &MoveArgs) -> Destination {
    Destination {
        parent: args.namespace.map(NamespaceId::new),
        segment: args.path.clone(), // clone: forge-db owns its request
    }
}

fn line(kind: &str, id: i64, moved: &MoveOutcome) -> String {
    format!(
        "kind\tid\told_path\tnew_path\ttombstones\tprojects\n{kind}\t{id}\t{}\t{}\t{}\t{}\n",
        moved.old_path, moved.new_path, moved.summary.nodes, moved.summary.projects
    )
}

pub fn move_project(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    project: i64,
    args: &MoveArgs,
) -> Result<String, ForgeError> {
    with_db(cfg, async |db| {
        move_project_with(db, request_id, project, args).await
    })
}

pub fn move_group(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    group: i64,
    args: &MoveArgs,
) -> Result<String, ForgeError> {
    with_db(cfg, async |db| {
        move_group_with(db, request_id, group, args).await
    })
}

pub fn rename_user(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    username: &str,
    new_username: &str,
    reclaim: bool,
) -> Result<String, ForgeError> {
    with_db(cfg, async |db| {
        rename_user_with(db, request_id, username, new_username, reclaim).await
    })
}

pub(crate) async fn move_project_with(
    db: &Db,
    request_id: &RequestId,
    project: i64,
    args: &MoveArgs,
) -> Result<String, ForgeError> {
    let moved = Hosting::new(db.clone()) // clone: the pool handle is shared
        .operator_move_project(
            &context(request_id),
            ProjectId::new(project),
            &destination(args),
            args.reclaim,
        )
        .await?;
    log_moved("project", project, &moved, request_id);
    Ok(line("project", project, &moved))
}

pub(crate) async fn move_group_with(
    db: &Db,
    request_id: &RequestId,
    group: i64,
    args: &MoveArgs,
) -> Result<String, ForgeError> {
    let moved = Hosting::new(db.clone()) // clone: the pool handle is shared
        .operator_move_group(
            &context(request_id),
            GroupId::new(group),
            &destination(args),
            args.reclaim,
        )
        .await?;
    log_moved("group", group, &moved, request_id);
    Ok(line("group", group, &moved))
}

pub(crate) async fn rename_user_with(
    db: &Db,
    request_id: &RequestId,
    username: &str,
    new_username: &str,
    reclaim: bool,
) -> Result<String, ForgeError> {
    let user = users::id_by_username(db, username).await?;
    let moved = Hosting::new(db.clone()) // clone: the pool handle is shared
        .operator_rename_user(&context(request_id), user, new_username, reclaim)
        .await?;
    log_moved("user", user.get(), &moved, request_id);
    Ok(line("user", user.get(), &moved))
}

/// The operator's move, logged once (ids and counts; paths stay in the
/// output and the database).
fn log_moved(kind: &'static str, id: i64, moved: &MoveOutcome, request_id: &RequestId) {
    tracing::info!(
        target: "forge_cli",
        request_id = %request_id,
        kind,
        id,
        namespace_id = moved.summary.namespace_id.get(),
        tombstones = moved.summary.nodes,
        projects = moved.summary.projects,
        "forge.namespace.moved_by_operator"
    );
}

#[cfg(test)]
mod tests;
