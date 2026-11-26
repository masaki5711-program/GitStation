//! A person's notes (plan 20 N1, ADR-0040 §6, option N-B):
//!
//! - `POST /api/v1/user/notes` (`PersonalNotesCreate` on the caller's own
//!   personal namespace: the person, `api`): create the notes — the
//!   **private** project `<user>/notes` — in ONE step whose pointer comes
//!   last (forge-hosting `notes` module docs): create (also
//!   `ProjectCreate` on the namespace), provision the repository now, the
//!   first commit on `main` (`.forge/doctypes/note.yml` with
//!   `owner_kind: user`, `README.md`; `forge_doc::note::seed`) recorded
//!   like a push, then the pointer (`forge.user_notes`). `201` with the
//!   notes. A refusal is `FG-KNOW-0006` (nothing written:
//!   `already_exists`, `name_taken`); a failure after the project was
//!   created is `FG-KNOW-0007` with `step` and `notes_project_id` — no
//!   pointer (runbook knowledge). A stale pointer (`FG-KNOW-0005`) is
//!   replaced.
//! - `GET /api/v1/user/notes` (`ProjectList` on the instance, then
//!   `ProjectRead` proved on the notes project with the caller's own
//!   principal): the caller's notes `{project_id, path, owner,
//!   default_branch}`. No notes yet: `404 FG-REPO-0005`; a pointer whose
//!   project was moved out of `<user>/notes` or is pending deletion: `404
//!   FG-KNOW-0005` (the caller's own pointer, read only after the
//!   registry's decision: it names nothing of another person's).
//!
//! A quick note is not a route of its own: it is one direct commit of
//! `forge_doc::note::quick_note` on the notes' `main` through the simple
//! editor's commit (`RepoPush`; `gstn note new`). Notes are a private
//! project, so the search (`ProjectRead` / `RepoBrowse` per hit) never
//! shows them to anyone without a role on it.

use std::sync::Arc;
use std::time::Duration;

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{
    prove, Action, InstanceCtx, LoadedParent, LoadedUserNamespace, ParentCtx, ProjectCtx,
};
use forge_core::{ObjectId, RepositoryState, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::notes::{
    missing, CreatedNotes, State as NotesState, NOTES_BRANCH, NOTES_SEGMENT,
};
use forge_hosting::Parent as HostedParent;
use serde::Serialize;

use crate::app::AppState;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::first_commit::{self, FirstCommit};
use crate::routes::git::SmartHttp;
use crate::routes::projects;
use crate::routes::write::{absent_because, bounded, hosting, json, no_store, write_request};

/// `GET` / `POST` the caller's notes.
pub const PATH: &str = "/api/v1/user/notes";
/// The route name in logs.
const ROUTE: &str = "notes_create";
/// The repository's creation (git init, configuration) as a whole.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(30);
/// Marking the repository ready.
const MARK_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// The caller's notes.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NotesBody {
    /// The notes project.
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// The notes project's full path (`<user>/notes`).
    pub path: String,
    /// The person's login: a note's `owner`.
    pub owner: String,
    /// `main`: quick notes are direct commits on it.
    pub default_branch: String,
}

/// The created notes.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatedNotesBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// `<user>/notes`.
    pub path: String,
    /// The person's login: a note's `owner`.
    pub owner: String,
    /// `main`.
    pub default_branch: String,
    /// The first commit (`.forge/doctypes/note.yml`, `README.md`).
    pub commit: String,
    /// The stale notes project the pointer named before, now replaced
    /// (`null` for first notes).
    #[schema(required = true, minimum = 1)]
    pub replaced_project_id: Option<i64>,
}

fn branch_name() -> String {
    NOTES_BRANCH
        .strip_prefix("refs/heads/")
        .unwrap_or(NOTES_BRANCH)
        .to_owned()
}

fn respond(
    ctx: &crate::Ctx,
    status: StatusCode,
    built: Result<serde_json::Value, ForgeError>,
) -> Response {
    match built {
        Ok(value) => no_store(status, value),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `POST`: the caller's own personal namespace (the parent of the notes).
pub struct OwnNamespace;

impl OwnNamespace {
    async fn lookup(
        state: &AppState,
        viewer: Option<UserId>,
    ) -> Result<Lookup<ParentCtx>, ForgeError> {
        let Some(me) = viewer else {
            return Ok(absent_because("notes_without_user"));
        };
        let hosting = hosting(state)?;
        let Some((namespace, _)) =
            bounded(state, "notes_namespace", hosting.own_namespace(me)).await?
        else {
            return Ok(absent_because("own_namespace_missing"));
        };
        let parent = bounded(state, "parent_load", hosting.parent(namespace, Some(me))).await?;
        let Some(HostedParent::User {
            namespace,
            owner,
            owner_kind,
        }) = parent
        else {
            return Ok(absent_because("own_namespace_not_personal"));
        };
        Ok(Lookup::Found {
            ctx: ParentCtx::new(LoadedParent::User(LoadedUserNamespace {
                namespace,
                owner,
                owner_kind,
            })),
            canonical: None,
            tombstone: None,
            project: None,
            group: None,
        })
    }
}

impl Loader<AppState> for OwnNamespace {
    type Ctx = ParentCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        _: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ParentCtx> {
        Box::pin(Self::lookup(state, viewer))
    }
}

/// `GET /api/v1/user/notes` → `200` the caller's notes. The registry
/// decided `ProjectList` on the instance (a signed-in, active caller with
/// `read_api`); everything about the notes is answered after that, and the
/// notes project itself is proved `ProjectRead` with the caller's own
/// principal before anything of it is shown.
pub async fn read(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let me = w.proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::NotFound).with_context("reason", "notes_without_user")
        })?;
        let hosting = hosting(&state)?;
        let project = match bounded(&state, "notes_load", hosting.notes_state(me)).await? {
            NotesState::None => {
                return Err(ForgeError::new(ErrorCode::NotFound).with_context("reason", "no_notes"))
            }
            NotesState::Stale { project, reason } => return Err(missing(reason, me, project)),
            NotesState::Live(project) => project,
        };
        let record = projects::load_bounded(&state, project, Some(me))
            .await?
            .ok_or_else(|| missing("absent", me, project))?;
        if record.id != project {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "loaded_another_project")
                .with_context("project_id", project.get())
                .with_context("loaded_project_id", record.id.get()));
        }
        prove(
            w.proof.principal(),
            Action::ProjectRead,
            ProjectCtx::new(projects::loaded(&record)),
        )
        .map(drop)?;
        let path = record.path.full_path();
        let owner = path
            .split_once('/')
            .map_or_else(|| path.clone(), |(ns, _)| ns.to_owned()); // clone: no parent segment
        json(NotesBody {
            project_id: record.id.get(),
            path,
            owner,
            default_branch: branch_name(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

fn smart(state: &AppState) -> Result<Arc<SmartHttp>, ForgeError> {
    state.smart_http.clone().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "smart_http_unconfigured")
    })
}

/// A failure after the notes project was created: `FG-KNOW-0007` with the
/// step and the half-made project; the cause chained.
fn provision_failed(
    step: &'static str,
    user: UserId,
    created: &CreatedNotes,
    cause: ForgeError,
) -> ForgeError {
    ForgeError::new(ErrorCode::NotesProvisionFailed)
        .with_context("step", step)
        .with_context("user_id", user.get())
        .with_context("notes_project_id", created.project.project_id.get())
        .with_context("repo_id", created.project.repository_id.get())
        .with_context("cause_code", cause.code().id())
        .with_source(cause)
}

/// Step 2: the repository, now (the queued job then finds it ready).
async fn provision(
    state: &AppState,
    smart: &SmartHttp,
    w: &WriteParts<ParentCtx, ()>,
    created: &CreatedNotes,
) -> Result<(), ForgeError> {
    let hosting = hosting(state)?;
    let repo = created.project.repository_id;
    let target = hosting.wiki_provision_target(repo).await?;
    match target.state {
        RepositoryState::Ready => return Ok(()),
        RepositoryState::Provisioning => {}
        other => {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "notes_repository_not_provisioning")
                .with_context("state", other.as_str()))
        }
    }
    let deadline = std::time::Instant::now() + PROVISION_TIMEOUT;
    smart
        .storage
        .init_bare_or_adopt_until(&smart.git, repo, target.token, None, deadline)
        .await?;
    match hosting
        .mark_notes_ready(&write_request(w), repo, MARK_READY_TIMEOUT)
        .await?
    {
        forge_hosting::wiki::MarkReady::Marked { .. }
        | forge_hosting::wiki::MarkReady::AlreadyReady { .. } => Ok(()),
        forge_hosting::wiki::MarkReady::Purging { .. } => {
            Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "notes_repository_purging"))
        }
    }
}

/// Step 3: the first commit, recorded like a push of it.
async fn seed(
    state: &AppState,
    smart: &SmartHttp,
    w: &WriteParts<ParentCtx, ()>,
    created: &CreatedNotes,
    user: UserId,
) -> Result<ObjectId, ForgeError> {
    let files = forge_doc::note::seed();
    let message = format!("Create the notes of {}", created.owner);
    first_commit::commit(
        state,
        smart,
        w,
        &FirstCommit {
            project: created.project.project_id,
            repo: created.project.repository_id,
            branch: NOTES_BRANCH,
            files: &files,
            message: &message,
            route: ROUTE,
        },
        user,
    )
    .await
}

/// `POST /api/v1/user/notes` → `201` the notes.
pub async fn create(state: AppState, request: Dispatched<ParentCtx>) -> Response {
    let w = request.into_write();
    let built = create_steps(&state, &w).await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

async fn create_steps(
    state: &AppState,
    w: &WriteParts<ParentCtx, ()>,
) -> Result<serde_json::Value, ForgeError> {
    let hosting = hosting(state)?;
    let smart = smart(state)?;
    let user = w.proof.principal().user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_user")
    })?;
    let request = write_request(w);
    // Step 1: refusals and the creation's own errors are answered as such.
    let created = hosting.create_notes_project(&request, &w.proof).await?;
    let fail = |step: &'static str| {
        let created = &created;
        move |e: ForgeError| provision_failed(step, user, created, e)
    };
    provision(state, &smart, w, &created)
        .await
        .map_err(fail("provision"))?;
    let commit = seed(state, &smart, w, &created, user)
        .await
        .map_err(fail("seed"))?;
    hosting
        .link_notes(&w.proof, &created)
        .await
        // The project exists by now: even a refusal here (a concurrent
        // request pointed first) leaves it behind, so it is FG-KNOW-0007
        // with the half-made project (the refusal's own code is chained).
        .map_err(fail("link"))?;
    tracing::info!(
        target: "forge_web",
        event = "forge.notes.created",
        request_id = %w.ctx.request_id,
        user_id = user.get(),
        notes_project_id = created.project.project_id.get(),
        replaced_project_id = created.replacing.map(|p| p.get()),
        repo_id = created.project.repository_id.get(),
        commit = %commit.as_str(),
        "personal notes created"
    );
    json(CreatedNotesBody {
        project_id: created.project.project_id.get(),
        path: format!("{}/{}", created.owner, NOTES_SEGMENT),
        owner: created.owner.clone(), // clone: the answer owns it
        default_branch: branch_name(),
        commit: commit.as_str().to_owned(),
        replaced_project_id: created.replacing.map(|p| p.get()),
    })
}
