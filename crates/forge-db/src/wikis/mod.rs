//! A project's wiki link (plan 20 W1, ADR-0040 §5; migration
//! 20270108100000): `forge.project_wikis`, one row per linked wiki.
//!
//! - **The inheritance** is not here: the role computation reads the table
//!   (`roles::chain_role_columns!`, the one place), so every loader, list
//!   and in-transaction re-read sees it.
//! - **A person's commands** (`ChangeTx`, audited on the owner project):
//!   [`link_decided`] (`project.wiki_linked`), [`unlink_decided`]
//!   (`project.wiki_unlinked`). Both lock the owner project and decide again
//!   on it (`decide`: the caller's reconfirmed `ProjectWikiLink`).
//! - **Reads**: [`link_of`] (either side of a link).
//!
//! A refusal is `FG-KNOW-0003` with its `reason` (`already_linked`,
//! `wiki_already_linked`, `chain`, `self`, `other_namespace`,
//! `wiki_pending_deletion`, `not_linked`); nothing is written then. The
//! table's constraints are the final word (a concurrent link loses on them,
//! never builds a chain).

use forge_core::ids::{ProjectId, UserId};
use forge_core::wiki::WikiLinkChange;
use forge_core::AuditDetails;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::{query_error, read_query_error, Db};

/// One stored link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    /// The owner project.
    pub project: ProjectId,
    /// The wiki project.
    pub wiki: ProjectId,
    pub linked_by: UserId,
    /// UTC, RFC 3339.
    pub linked_at: String,
}

type Row = (i64, i64, i64, String);

const SELECT: &str = "SELECT project_id, wiki_project_id, linked_by, \
     to_char(linked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
     FROM forge.project_wikis";

fn parse(row: Row) -> WikiLink {
    let (project, wiki, by, at) = row;
    WikiLink {
        project: ProjectId::new(project),
        wiki: ProjectId::new(wiki),
        linked_by: UserId::new(by),
        linked_at: at,
    }
}

/// `FG-KNOW-0003` with its `reason` and the owner project.
pub fn refused(reason: &'static str, project: ProjectId) -> ForgeError {
    ForgeError::new(ErrorCode::WikiLinkRefused)
        .with_context("reason", reason)
        .with_context("project_id", project.get())
}

/// The link `project` is part of, as owner or as wiki (read pool).
pub async fn link_of(db: &Db, project: ProjectId) -> Result<Option<WikiLink>, ForgeError> {
    const QUERY: &str = "wikis.link_of";
    db.within(QUERY, async {
        let row: Option<Row> = sqlx::query_as(&format!(
            "{SELECT} WHERE project_id = $1 OR wiki_project_id = $1"
        ))
        .bind(project.get())
        .fetch_optional(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(row.map(parse))
    })
    .await
}

/// Where a new wiki of `project` goes: the parent namespace of the
/// project's namespace and the project's own segment (read pool). `None`
/// when the project does not exist.
pub async fn owner_placement(
    db: &Db,
    project: ProjectId,
) -> Result<Option<(forge_core::ids::NamespaceId, String)>, ForgeError> {
    const QUERY: &str = "wikis.owner_placement";
    db.within(QUERY, async {
        let row: Option<(Option<i64>, String)> = sqlx::query_as(
            "SELECT parent_id, segment FROM forge.namespaces \
             WHERE kind = 'project' AND owner_id = $1",
        )
        .bind(project.get())
        .fetch_optional(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        match row {
            None => Ok(None),
            Some((Some(parent), segment)) => {
                Ok(Some((forge_core::ids::NamespaceId::new(parent), segment)))
            }
            // A project namespace always has a parent (CHECK): corrupt data.
            Some((None, _)) => Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "project_namespace_without_parent")
                .with_context("project_id", project.get())),
        }
    })
    .await
}

/// The link of either side, read on the write transaction (no lock: the
/// insert's constraints decide a race).
async fn link_on(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<Option<WikiLink>, ForgeError> {
    let row: Option<Row> = sqlx::query_as(&format!(
        "{SELECT} WHERE project_id = $1 OR wiki_project_id = $1"
    ))
    .bind(project.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("wikis.link_on", e))?;
    Ok(row.map(parse))
}

/// The parent namespace of `project`'s namespace and whether it is pending
/// deletion; `None` when the project does not exist.
async fn placement(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<Option<(Option<i64>, bool)>, ForgeError> {
    sqlx::query_as(
        "SELECT n.parent_id, p.deleted_at IS NOT NULL FROM forge.projects p \
         JOIN forge.namespaces n ON n.kind = 'project' AND n.owner_id = p.id \
         WHERE p.id = $1 FOR SHARE OF p",
    )
    .bind(project.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("wikis.placement", e))
}

fn record(change: WikiLinkChange) -> ChangeRecord {
    let details = AuditDetails::WikiLink(change);
    ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(change.project.get()),
            project_id: Some(change.project),
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

/// A unique or check violation of the table as `FG-KNOW-0003`; anything
/// else stays a query error.
fn insert_error(project: ProjectId, e: sqlx::Error) -> ForgeError {
    let reason = match e.as_database_error().and_then(|d| d.constraint()) {
        Some("project_wikis_pkey") => "already_linked",
        Some("project_wikis_wiki_once") => "wiki_already_linked",
        Some("project_wikis_not_self") => "self",
        Some("project_wikis_no_chain") => "chain",
        _ => return query_error("wikis.link", e),
    };
    refused(reason, project)
}

/// Link `wiki` to `owner` for `caller`, decided again on the locked owner
/// project (`decide`: the caller's reconfirmed `ProjectWikiLink`). The
/// wiki must live in the owner's namespace and not be pending deletion.
/// One `project.wiki_linked` audit row on the owner project. Refusals
/// (module docs) write nothing.
pub async fn link_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    owner: ProjectId,
    wiki: ProjectId,
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock_shared(&mut tx, owner).await?;
        let decided = decide(&locked_access_facts(&mut tx, owner, caller, &locked).await?)?;
        if owner == wiki {
            return Err(refused("self", owner));
        }
        let owner_place = tx
            .step(|c| async move { placement(c, owner).await })
            .await?;
        let wiki_place = tx.step(|c| async move { placement(c, wiki).await }).await?;
        let (Some((owner_parent, _)), Some((wiki_parent, wiki_deleted))) =
            (owner_place, wiki_place)
        else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "project")
                .with_context("reason", "absent"));
        };
        if owner_parent != wiki_parent {
            return Err(
                refused("other_namespace", owner).with_context("wiki_project_id", wiki.get())
            );
        }
        if wiki_deleted {
            return Err(
                refused("wiki_pending_deletion", owner).with_context("wiki_project_id", wiki.get())
            );
        }
        // The early answers (the constraints still decide a race).
        if let Some(existing) = tx.step(|c| async move { link_on(c, owner).await }).await? {
            let reason = if existing.project == owner {
                "already_linked"
            } else {
                "chain"
            };
            return Err(refused(reason, owner).with_context("wiki_project_id", wiki.get()));
        }
        if let Some(existing) = tx.step(|c| async move { link_on(c, wiki).await }).await? {
            let reason = if existing.wiki == wiki {
                "wiki_already_linked"
            } else {
                "chain"
            };
            return Err(refused(reason, owner).with_context("wiki_project_id", wiki.get()));
        }
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.project_wikis (project_id, wiki_project_id, linked_by) \
                 VALUES ($1, $2, $3)",
            )
            .bind(owner.get())
            .bind(wiki.get())
            .bind(caller.get())
            .execute(c)
            .await
            .map_err(|e| insert_error(owner, e))
            .map(drop)
        })
        .await?;
        let change = WikiLinkChange {
            project: owner,
            wiki,
            linked: true,
        };
        Ok(Settle::Commit(record(change), decided))
    }
    .await;
    tx.settle("project.wiki_linked", outcome).await
}

/// Unlink `owner`'s wiki for `caller`, decided again on the locked owner
/// project. The wiki project stays (its history, members and content);
/// the owner's members lose the inherited role with the commit. One
/// `project.wiki_unlinked` audit row on the owner project; `FG-KNOW-0003
/// reason=not_linked` when it has no wiki. Returns the unlinked wiki.
pub async fn unlink_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    owner: ProjectId,
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(ProjectId, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock_shared(&mut tx, owner).await?;
        let decided = decide(&locked_access_facts(&mut tx, owner, caller, &locked).await?)?;
        // Writes that decided on the inherited role hold the links' lock
        // shared until they commit (`roles::lock_roles`): wait for them.
        tx.step(|c| async move {
            sqlx::query(crate::roles::LOCK_WIKI_LINKS)
                .execute(c)
                .await
                .map_err(|e| query_error("wikis.unlink.lock", e))
                .map(drop)
        })
        .await?;
        let removed: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "DELETE FROM forge.project_wikis WHERE project_id = $1 \
                     RETURNING wiki_project_id",
                )
                .bind(owner.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("wikis.unlink", e))
            })
            .await?;
        let Some(wiki) = removed.map(ProjectId::new) else {
            return Err(refused("not_linked", owner));
        };
        let change = WikiLinkChange {
            project: owner,
            wiki,
            linked: false,
        };
        Ok(Settle::Commit(record(change), (wiki, decided)))
    }
    .await;
    tx.settle("project.wiki_unlinked", outcome).await
}

#[cfg(test)]
mod tests;
