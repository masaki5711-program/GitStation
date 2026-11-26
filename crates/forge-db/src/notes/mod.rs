//! A person's notes pointer (plan 20 N1, ADR-0040 §6, option N-B;
//! migration 20270111100000): `forge.user_notes`, one row per person,
//! naming the private project `<user>/notes` that holds their notes.
//!
//! - **The project** is an ordinary project, created by the person in their
//!   own namespace (`project.created`, audited); reading and writing notes
//!   are the existing actions on it (`RepoBrowse`, `RepoPush`). Nothing
//!   here decides access: the pointer only says which project is the
//!   person's notes.
//! - **Reads**: [`notes_of`] — the pointer and whether its project is still
//!   the person's live `<user>/notes` ([`NotesState`]).
//! - **Writes**: [`link`] — written last by the creation (after the
//!   project, its repository and its first commit exist), replacing only a
//!   stale pointer the caller saw ([`NotesState::Stale`]); anything else is
//!   `FG-KNOW-0006 reason=already_exists` and writes nothing.
//! - The purge of the project removes its row (`projects::purge`).

use forge_core::ids::{NamespaceId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use crate::{query_error, read_query_error, Db};

/// The notes project's segment in the person's namespace.
pub const NOTES_SEGMENT: &str = "notes";

/// What a person's pointer names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotesState {
    /// No notes yet.
    None,
    /// The live `<user>/notes`.
    Live(ProjectId),
    /// A project that is no longer the person's live notes: `reason` is
    /// `deleted` (pending deletion) or `moved` (no longer
    /// `<user>/notes`). The pointer stays until the next creation replaces
    /// it (`FG-KNOW-0005`).
    Stale {
        project: ProjectId,
        reason: &'static str,
    },
}

/// `FG-KNOW-0006` with its `reason` and the person.
pub fn refused(reason: &'static str, user: UserId) -> ForgeError {
    ForgeError::new(ErrorCode::NotesCreateRefused)
        .with_context("reason", reason)
        .with_context("user_id", user.get())
}

/// `FG-KNOW-0005` for a stale pointer.
pub fn missing(reason: &'static str, user: UserId, project: ProjectId) -> ForgeError {
    ForgeError::new(ErrorCode::NotesProjectMissing)
        .with_context("reason", reason)
        .with_context("user_id", user.get())
        .with_context("notes_project_id", project.get())
}

/// The person's own namespace and its segment (their login; read pool);
/// `None` for an unknown user.
pub async fn own_namespace(
    db: &Db,
    user: UserId,
) -> Result<Option<(NamespaceId, String)>, ForgeError> {
    const QUERY: &str = "notes.own_namespace";
    db.within(QUERY, async {
        let row: Option<(i64, String)> = sqlx::query_as(
            "SELECT id, segment FROM forge.namespaces WHERE kind = 'user' AND owner_id = $1",
        )
        .bind(user.get())
        .fetch_optional(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(row.map(|(id, segment)| (NamespaceId::new(id), segment)))
    })
    .await
}

/// The person's pointer and its project's state (read pool).
pub async fn notes_of(db: &Db, user: UserId) -> Result<NotesState, ForgeError> {
    const QUERY: &str = "notes.notes_of";
    db.within(QUERY, async {
        // (project, pending deletion, still `<user>/notes`)
        let row: Option<(i64, bool, bool)> = sqlx::query_as(
            "SELECT un.project_id, p.deleted_at IS NOT NULL, \
                    COALESCE(pn.parent_id = uns.id AND pn.segment = $2, false) \
             FROM forge.user_notes un \
             JOIN forge.projects p ON p.id = un.project_id \
             LEFT JOIN forge.namespaces pn ON pn.kind = 'project' AND pn.owner_id = p.id \
             LEFT JOIN forge.namespaces uns ON uns.kind = 'user' AND uns.owner_id = un.user_id \
             WHERE un.user_id = $1",
        )
        .bind(user.get())
        .bind(NOTES_SEGMENT)
        .fetch_optional(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(match row {
            None => NotesState::None,
            Some((id, true, _)) => NotesState::Stale {
                project: ProjectId::new(id),
                reason: "deleted",
            },
            Some((id, false, false)) => NotesState::Stale {
                project: ProjectId::new(id),
                reason: "moved",
            },
            Some((id, false, true)) => NotesState::Live(ProjectId::new(id)),
        })
    })
    .await
}

/// Point `user`'s notes at `project` (write pool, one statement). With
/// `replacing: None` only a person without a pointer gets one; with
/// `Some(stale)` only the pointer that still names `stale` is replaced (a
/// concurrent creation that wrote first wins). Otherwise `FG-KNOW-0006
/// reason=already_exists`; nothing is written.
pub async fn link(
    db: &Db,
    user: UserId,
    project: ProjectId,
    replacing: Option<ProjectId>,
) -> Result<(), ForgeError> {
    const QUERY: &str = "notes.link";
    db.within(QUERY, async {
        let done = sqlx::query(
            "INSERT INTO forge.user_notes (user_id, project_id) VALUES ($1, $2) \
             ON CONFLICT (user_id) DO UPDATE \
               SET project_id = EXCLUDED.project_id, created_at = now() \
               WHERE forge.user_notes.project_id = $3",
        )
        .bind(user.get())
        .bind(project.get())
        .bind(replacing.map(ProjectId::get))
        .execute(db.write_pool())
        .await
        .map_err(|e| {
            if e.as_database_error().and_then(|d| d.constraint()) == Some("user_notes_project_once")
            {
                refused("already_exists", user).with_context("notes_project_id", project.get())
            } else {
                query_error(QUERY, e)
            }
        })?;
        if done.rows_affected() == 1 {
            Ok(())
        } else {
            Err(refused("already_exists", user))
        }
    })
    .await
}

#[cfg(test)]
mod tests;
