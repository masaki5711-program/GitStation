//! A project's wiki (plan 20 W2, ADR-0040 §5, option W-C):
//!
//! - `POST /api/v1/projects/{id}/wiki` (`ProjectWikiLink` on the owner:
//!   Maintainer, people only, `api`): create the wiki — an ordinary project
//!   `<owner>-wiki` (or `path`) in the owner's namespace — and link it, in
//!   ONE step whose link comes last (forge-hosting `wiki` module docs):
//!   create (also `ProjectCreate` on the namespace), provision the
//!   repository now, the first commit on `main` (`.forge/doctypes/
//!   wiki.page.yml`, `WIKI-0001.md`, `forge_doc::wiki::seed`) recorded like
//!   a push, protect `main` (Developer may push, no force push or delete),
//!   link. `201` with the wiki. A refusal is `FG-KNOW-0003` (nothing
//!   written: `already_linked`, `chain`, `name_taken`, …); a failure after
//!   the project was created is `FG-KNOW-0004` with `step` and
//!   `wiki_project_id` — **no link**, so nobody inherits a role on the
//!   half-made project (runbook knowledge).
//! - `DELETE …/wiki` (`ProjectWikiLink`): unlink; the wiki project stays.
//!   `200`; `FG-KNOW-0003 reason=not_linked` without a wiki.

use std::sync::Arc;
use std::time::Duration;

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::ProjectCtx;
use forge_core::{
    parse_positive_id, ObjectId, ProjectId, PushApplied, PushRecord, PushUpdateKind, PushedCommits,
    PushedRef, Pusher, RefName, RepoId, RepositoryState, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::file_commit::{FilePath, RootFiles};
use forge_hosting::wiki::{CreatedWiki, MarkReady, NewWikiFields, WIKI_BRANCH};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::git::SmartHttp;
use crate::routes::git_push::{record_applied, PushLog};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, hosting, json, no_store, write_request};

/// `POST` / `DELETE` a project's wiki.
pub const ONE: &str = "/api/v1/projects/{id}/wiki";
/// The body cap.
pub const MAX_BODY: usize = 4 * 1024;
/// The route name in logs.
const ROUTE: &str = "wiki_create";
/// Each git step of the first commit.
const GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// The repository's creation (git init, configuration) as a whole.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(30);
/// Marking the repository ready.
const MARK_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// `POST …/wiki` (both fields optional).
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateWiki {
    /// The wiki project's path segment (ADR-0015 rules); default
    /// `<owner segment>-wiki`.
    #[serde(default)]
    pub path: Option<String>,
    /// Default `<owner display name> Wiki`.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// The created wiki.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiBody {
    /// The owner project.
    #[schema(minimum = 1)]
    pub project_id: i64,
    #[schema(minimum = 1)]
    pub wiki_project_id: i64,
    /// The wiki project's full path.
    pub wiki_path: String,
    pub display_name: String,
    /// `main`.
    pub default_branch: String,
    /// The first commit (`.forge/doctypes/wiki.page.yml`, `WIKI-0001.md`).
    pub commit: String,
    /// `WIKI-0001.md`.
    pub first_page: String,
}

/// The unlinked wiki (the project stays).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WikiUnlinkedBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    #[schema(minimum = 1)]
    pub wiki_project_id: i64,
}

fn project_id(raw_path: &str) -> Result<ProjectId, ForgeError> {
    let bad = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(bad)?;
    let (project, tail) = rest.split_once('/').ok_or_else(bad)?;
    if tail != "wiki" {
        return Err(bad());
    }
    parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)
}

/// The owner project of the route, loaded as `GET /api/v1/projects/{id}`.
pub struct WikiOwner;

impl WikiOwner {
    async fn lookup(
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<ProjectCtx>, ForgeError> {
        let id = project_id(parts.uri.path())?;
        let Some(record) = projects::load_bounded(state, id, viewer).await? else {
            return Ok(absent());
        };
        if record.id != id {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "loaded_another_project")
                .with_context("project_id", id.get())
                .with_context("loaded_project_id", record.id.get()));
        }
        Ok(Lookup::Found {
            ctx: ProjectCtx::new(projects::loaded(&record)),
            canonical: None,
            tombstone: None,
            project: Some(record),
            group: None,
        })
    }
}

impl Loader<AppState> for WikiOwner {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(Self::lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, CreateWiki> for WikiOwner {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a CreateWiki,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(Self::lookup(state, parts, viewer))
    }
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

fn smart(state: &AppState) -> Result<Arc<SmartHttp>, ForgeError> {
    state.smart_http.clone().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "smart_http_unconfigured")
    })
}

/// A failure after the wiki project was created: `FG-KNOW-0004` with the
/// step and the half-made project; the cause chained.
fn provision_failed(
    step: &'static str,
    owner: ProjectId,
    wiki: ProjectId,
    repo: RepoId,
    cause: ForgeError,
) -> ForgeError {
    ForgeError::new(ErrorCode::WikiProvisionFailed)
        .with_context("step", step)
        .with_context("project_id", owner.get())
        .with_context("wiki_project_id", wiki.get())
        .with_context("repo_id", repo.get())
        .with_context("cause_code", cause.code().id())
        .with_source(cause)
}

/// Step 2: the repository, now (the queued job then finds it ready).
async fn provision(
    state: &AppState,
    smart: &SmartHttp,
    w: &WriteParts<ProjectCtx, CreateWiki>,
    repo: RepoId,
) -> Result<(), ForgeError> {
    let hosting = hosting(state)?;
    let target = hosting.wiki_provision_target(repo).await?;
    match target.state {
        RepositoryState::Ready => return Ok(()),
        RepositoryState::Provisioning => {}
        other => {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "wiki_repository_not_provisioning")
                .with_context("state", other.as_str()))
        }
    }
    let deadline = std::time::Instant::now() + PROVISION_TIMEOUT;
    smart
        .storage
        .init_bare_or_adopt_until(&smart.git, repo, target.token, None, deadline)
        .await?;
    match hosting
        .mark_wiki_ready(&write_request(w), repo, MARK_READY_TIMEOUT)
        .await?
    {
        MarkReady::Marked { .. } | MarkReady::AlreadyReady { .. } => Ok(()),
        MarkReady::Purging { .. } => {
            Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "wiki_repository_purging"))
        }
    }
}

/// Step 3: the first commit, recorded like a push of it.
async fn seed(
    state: &AppState,
    smart: &SmartHttp,
    w: &WriteParts<ProjectCtx, CreateWiki>,
    created: &CreatedWiki,
    user: UserId,
) -> Result<ObjectId, ForgeError> {
    let repo = created.project.repository_id;
    let review = state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })?;
    let author = review.commit_identity(user).await?;
    let seeded = forge_doc::wiki::seed(&created.display_name, &created.owner_namespace);
    let files = seeded
        .iter()
        .map(|(path, content)| Ok((FilePath::parse(path)?, content.as_bytes())))
        .collect::<Result<Vec<_>, ForgeError>>()?;
    let branch = RefName::parse(WIKI_BRANCH)?;
    let message = format!("Create the wiki of {}", created.owner_namespace);
    let commit = smart
        .git
        .commit_root_files(
            &smart.storage,
            repo,
            &RootFiles {
                branch: &branch,
                files: &files,
                message: &message,
                author: &author,
            },
            GIT_TIMEOUT,
        )
        .await;
    // Whatever the outcome, the ref may have changed.
    smart.browse.invalidate(repo);
    let commit = commit?;
    if let Some(push) = smart.push.clone() {
        let record = PushRecord {
            repository_id: repo,
            updates: vec![PushedRef {
                name: branch,
                old: ObjectId::parse(&"0".repeat(commit.as_str().len()))?,
                new: commit.clone(), // clone: the record owns its ids
                kind: PushUpdateKind::Create,
                commits: vec![commit.clone()], // clone: as above
                total_commits: 1,
                total_commits_partial: false,
            }],
        };
        let pusher = Pusher {
            request_id: w.ctx.request_id.clone(), // clone: the record keeps its own
            user,
            kind: w.proof.principal().account_kind(),
            credential_id: w.credential_id,
            source_ip: None,
            user_agent: None,
        };
        let log = PushLog {
            request_id: w.ctx.request_id.clone(), // clone: as above
            project_id: created.project.project_id,
            repo_id: repo.get(),
            lang: w.ctx.lang,
            route: ROUTE,
        };
        let line = PushApplied::new(&pusher, created.project.project_id, &record);
        if let Err(err) = line.log_as("final", ROUTE) {
            log.failure(err);
        }
        let commits = PushedCommits {
            commits: vec![commit.clone()], // clone: the record task owns it
            complete: true,
        };
        let recorder = Arc::clone(&push.recorder);
        let guard = smart.push_tasks.start();
        let recorded = tokio::spawn(async move {
            let _guard = guard;
            record_applied(&*recorder, &pusher, &record, &commits, &log).await;
        });
        if let Err(e) = recorded.await {
            tracing::warn!(
                target: "forge_web",
                event = "forge.wiki.seed_record_failed",
                request_id = %w.ctx.request_id,
                error_code = ErrorCode::PushRecordFailed.id(),
                detail = %e,
                "the first commit of a wiki could not be recorded as a push"
            );
        }
    }
    Ok(commit)
}

/// `POST …/wiki` → `201` the wiki.
pub async fn create(state: AppState, request: Dispatched<ProjectCtx, CreateWiki>) -> Response {
    let w = request.into_write();
    let built = create_steps(&state, &w).await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

async fn create_steps(
    state: &AppState,
    w: &WriteParts<ProjectCtx, CreateWiki>,
) -> Result<serde_json::Value, ForgeError> {
    let hosting = hosting(state)?;
    let smart = smart(state)?;
    let owner = *w.proof.id();
    let user = w.proof.principal().user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_user")
    })?;
    let fields = NewWikiFields {
        path: w.input.path.clone(), // clone: forge-hosting owns its request
        display_name: w.input.display_name.clone(),
    };
    let request = write_request(w);
    // Step 1: refusals and the creation's own errors are answered as such.
    let created = hosting
        .create_wiki_project(&request, &w.proof, &fields)
        .await?;
    let wiki = created.project.project_id;
    let repo = created.project.repository_id;
    let fail =
        |step: &'static str| move |e: ForgeError| provision_failed(step, owner, wiki, repo, e);
    provision(state, &smart, w, repo)
        .await
        .map_err(fail("provision"))?;
    let commit = seed(state, &smart, w, &created, user)
        .await
        .map_err(fail("seed"))?;
    hosting
        .protect_wiki_main(&request, &w.proof, wiki)
        .await
        .map_err(fail("protect"))?;
    hosting
        .link_wiki(&request, &w.proof, wiki)
        .await
        // The project exists by now: even a refusal here (a concurrent
        // request linked another wiki first) leaves it behind, so it is
        // FG-KNOW-0004 with the half-made project, never "nothing written"
        // (the refusal's own code is chained).
        .map_err(fail("link"))?;
    tracing::info!(
        target: "forge_web",
        event = "forge.wiki.created",
        request_id = %w.ctx.request_id,
        project_id = owner.get(),
        wiki_project_id = wiki.get(),
        repo_id = repo.get(),
        commit = %commit.as_str(),
        "wiki created and linked"
    );
    let path = hosting
        .project(wiki, Some(user))
        .await?
        .map(|r| r.path.full_path())
        .unwrap_or_default();
    json(WikiBody {
        project_id: owner.get(),
        wiki_project_id: wiki.get(),
        wiki_path: path,
        display_name: created.display_name,
        default_branch: "main".to_owned(),
        commit: commit.as_str().to_owned(),
        first_page: forge_doc::wiki::FIRST_PAGE_PATH.to_owned(),
    })
}

/// `DELETE …/wiki` → `200` the unlinked wiki.
pub async fn unlink(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let wiki = hosting(&state)?
            .unlink_wiki(&write_request(&w), &w.proof)
            .await?;
        json(WikiUnlinkedBody {
            project_id: w.proof.id().get(),
            wiki_project_id: wiki.get(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}
