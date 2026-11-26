//! A person's notes (plan 20 N1, ADR-0040 §6, option N-B): an ordinary
//! **private** project `<user>/notes` in the person's own namespace,
//! pointed at by `forge.user_notes`. The route (`POST /api/v1/user/notes`)
//! runs the steps in order and writes the pointer **last**, so a failure
//! leaves none:
//!
//! 1. [`Hosting::create_notes_project`] — the registry proved
//!    `PersonalNotesCreate` on the caller's own namespace; the project is
//!    then created as `ProjectCreate` on that namespace, proved here with
//!    the caller's own principal (`forge_auth::prove`) and decided again
//!    inside the create transaction (`create_project`). A live pointer is
//!    `FG-KNOW-0006 reason=already_exists`, a taken path
//!    `reason=name_taken`; a stale one (moved or deleted project,
//!    `FG-KNOW-0005`) is replaced by the new project.
//! 2. the repository is created now (as the wiki's: `wiki_provision_target`
//!    and [`Hosting::mark_notes_ready`]).
//! 3. the first commit (the route: git).
//! 4. [`Hosting::link_notes`] — the pointer, replacing only the stale
//!    pointer step 1 saw.
//!
//! No protection is set: the person is the project's Owner and its only
//! member, and quick notes are direct commits on `main`.

use forge_auth::{prove, Action, Authorized, ParentCtx};
use forge_core::{ProjectId, RepoId, UserId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::notes::{self, NotesState};
pub use forge_db::notes::{missing, NotesState as State, NOTES_SEGMENT};
use forge_db::repositories::{self, MarkReady};
use forge_errors::{ErrorCode, ForgeError};

use crate::{caller, require, CreatedProject, Hosting, NewProjectFields, WriteRequest};

/// The branch the notes are created on.
pub const NOTES_BRANCH: &str = "refs/heads/main";

/// The notes project as created (step 1).
#[derive(Debug, Clone)]
pub struct CreatedNotes {
    pub project: CreatedProject,
    /// The stale notes project the pointer named (replaced at step 4).
    pub replacing: Option<ProjectId>,
    /// The person's login (the namespace segment; a note's `owner`).
    pub owner: String,
}

impl Hosting {
    /// What `user`'s notes pointer names (`GET /api/v1/user/notes`).
    pub async fn notes_state(&self, user: UserId) -> Result<NotesState, ForgeError> {
        notes::notes_of(&self.db, user).await
    }

    /// The caller's own personal namespace (the notes route's loader).
    pub async fn own_namespace(
        &self,
        user: UserId,
    ) -> Result<Option<(forge_core::NamespaceId, String)>, ForgeError> {
        notes::own_namespace(&self.db, user).await
    }

    /// Step 1 (module docs): create `<user>/notes` for the owner of
    /// `proof` (`PersonalNotesCreate` on their own namespace), private.
    pub async fn create_notes_project(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ParentCtx>,
    ) -> Result<CreatedNotes, ForgeError> {
        require(proof.action(), Action::PersonalNotesCreate)?;
        let user = caller(proof)?;
        let replacing = match notes::notes_of(&self.db, user).await? {
            NotesState::None => None,
            NotesState::Live(project) => {
                return Err(notes::refused("already_exists", user)
                    .with_context("notes_project_id", project.get()))
            }
            NotesState::Stale { project, .. } => Some(project),
        };
        // The caller's own decision to create in that namespace.
        let create = prove(
            proof.principal(),
            Action::ProjectCreate,
            proof.ctx().clone(),
        )?;
        let owner = match notes::own_namespace(&self.db, user).await? {
            Some((namespace, segment)) if namespace == *proof.id() => segment,
            _ => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "own_namespace_mismatch")
                    .with_context("user_id", user.get())
                    .with_context("namespace_id", proof.id().get()))
            }
        };
        let new = NewProjectFields {
            path: notes::NOTES_SEGMENT.to_owned(),
            display_name: "Notes".to_owned(),
            description: format!("Personal notes of {owner}"),
            visibility: Visibility::Private,
        };
        let project = self
            .create_project(request, &create, &new)
            .await
            .map_err(|e| {
                if e.code() == ErrorCode::NamespacePathTaken {
                    notes::refused("name_taken", user).with_source(e)
                } else {
                    e
                }
            })?;
        Ok(CreatedNotes {
            project,
            replacing,
            owner,
        })
    }

    /// Step 2: mark the notes repository ready (audited,
    /// `repository.created`; a no-op when the job was faster), bounded by
    /// `timeout`.
    pub async fn mark_notes_ready(
        &self,
        request: &WriteRequest,
        repo: RepoId,
        timeout: std::time::Duration,
    ) -> Result<MarkReady, ForgeError> {
        let mut ctx = ChangeContext::new(Actor::System, request.request_id.clone());
        ctx.actor_label = Some("forge-web.notes".to_owned());
        repositories::mark_ready_within(&self.db, &ctx, repo, timeout).await
    }

    /// Step 4: point the owner of `proof` at `created` (replacing only the
    /// stale pointer step 1 saw; `FG-KNOW-0006` otherwise).
    pub async fn link_notes(
        &self,
        proof: &Authorized<ParentCtx>,
        created: &CreatedNotes,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::PersonalNotesCreate)?;
        let user = caller(proof)?;
        notes::link(
            &self.db,
            user,
            created.project.project_id,
            created.replacing,
        )
        .await
    }
}
