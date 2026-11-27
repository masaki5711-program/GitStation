//! The agent builder's update merge requests (plan 16 A9, ADR-0032
//! Amendment A9).
//!
//! **The update** ([`Marketplace::plan_update`], read only): from the lock
//! at the default branch's tip, every locked part moves to its newest
//! stable version that is not revoked (forge-agent's rules-only
//! [`recommend`] — the base model and what the parts require are followed
//! the same way), never down: a locked version that is newer than the
//! newest stable one and still published is kept, and so is a locked part
//! with no stable version at all (a beta someone picked). A locked version
//! that was revoked and has no replacement is dropped (the build removes
//! its files; a part that requires it makes the build fail with
//! `FG-AGENT-0008`, never a silent half update). With `adapter` the base
//! model changes — the **generation migration**: the lock's other base
//! model is not kept, the new one's requirements are followed. Nothing to
//! change is `FG-AGENT-0015` (`no_lock`, `up_to_date`). The plan is then
//! the A7 builder's own ([`Marketplace::plan_build`]: digests, signatures,
//! selection, merge).
//!
//! **The job** ([`AgentUpdater::run`], `agent.update_merge_request`, in
//! `forge serve`): asked by a person (`POST …/agent-builder/update`,
//! origin `update`) or queued in the revocation's own transaction for
//! every project whose finished builds selected the revoked version
//! (origin `revocation`). It is opened **for a person** — the one who
//! asked, or for a revocation the one who asked for the project's newest
//! finished build — decided with that person's current rights before git
//! writes anything reachable: `AgentBuilderCompile` (a person, a
//! Developer), the branch's create as their push (`authorize_ref_updates`,
//! the protected-ref rules) and `MergeRequestCreate` (decided again inside
//! the open's transaction by forge-review's `Review::open`, which the
//! caller hands in as an [`UpdateOpener`]: only forge-review opens merge
//! requests, and feature crates never depend on each other). Nobody allowed is `FG-AGENT-0016` (the job
//! dies with it, logged once by the job runner: a revoked part left in
//! use is never silent).
//!
//! **Once.** The A7 branch `agent-builder/<12 hex of the lock digest>`
//! names the update: an open merge request from it is the update already
//! opened (nothing new; its event is recorded again, deduplicated by the
//! merge request), a branch whose tip holds exactly this build is reused
//! (an interrupted run), another branch of the name is `FG-GIT-0027
//! branch_exists`, a merge request a person closed is never reopened
//! (`FG-AGENT-0016 closed_by_person`). A revocation enqueues its jobs with
//! dedup `agent_update_revoked:<version_id>:<project_id>`.
//!
//! **Order of writes** (as the A7 build and the revert draft): the bot's
//! commit and ONE `update-ref` create (A7's `commit_tree_change`, the
//! secret rules on every written file first); the merge request (on a
//! failure, unless it is found open after all, the new branch is deleted
//! with a compare-and-swap); the push (`repository.pushed`, actor
//! `system` — the merge request's version and checks follow), the build's
//! record (`forge.agent_builds`, origin `update` / `revocation`) and the
//! event `merge_request.agent_update_opened` (its author is told:
//! notification kind `mr_agent_update`).
//!
//! The merge request is never merged without a person, as A7's:
//! `forge/agent-lock` verifies it, `forge/risk` names it
//! `agent_config_locked`, and auto-merge waits for a Maintainer who is not
//! its author.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use forge_agent::{recommend, CatalogVersion, Lock, Version, DEFAULT_ADAPTER};
use forge_auth::{
    authorize_ref_updates, prove, Action, Authorized, Credential, LoadedMergeRequest,
    LoadedProject, MergeRequestCtx, MergeRequestState, Principal, ProjectCtx, RefUpdate,
    RefUpdateCtx,
};
use forge_core::{
    telemetry, AgentUpdateOrigin, MergeRequestChange, MergeRequestId, ObjectId, ProjectId,
    ProjectRepository, PushRecord, PushUpdateKind, PushedCommits, PushedRef, RefName, RepoId,
    RepositoryState, RequestId, UserId,
};
use forge_db::agent_builder::{self as builds, AgentUpdateRequest, NewBuild};
use forge_db::change::{Actor, ChangeContext};
use forge_db::review;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::commit::CommitIdentity;
use forge_git::file_commit::{FilePath, TreeChange, COMMIT_DEADLINE};
use forge_git::secret_scan::{self, SecretScan};
use forge_git::{Git, RefChange, StorageRoot};

use crate::builder::{builder_branch, BuildPlan, Selected, MAX_TIP_BYTES, MAX_TIP_FILES};
use crate::{bounded, problems_error, Marketplace, BUILDER_EMAIL, BUILDER_NAME};

/// The most builders a revocation's update is tried for (newest first).
pub const MAX_CANDIDATES: i64 = 5;

/// The audit label of the update job's writes (actor `system`).
pub const UPDATE_ACTOR_LABEL: &str = "forge-agent-builder";
/// The bound of one git step of the job (a ref read, the branch's undo).
pub const UPDATE_GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// One part of the update: the locked version and the one the update
/// selects (`None`: dropped — a revoked version without a replacement, or
/// the base model the generation migration leaves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateLine {
    pub slug: String,
    pub kind: String,
    /// `None`: a part the update adds (a new requirement, a new base model).
    pub locked: Option<String>,
    /// The locked version is revoked (or no longer published).
    pub locked_revoked: bool,
    pub next: Option<String>,
}

/// What an update would build ([`Marketplace::plan_update`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePlan {
    pub lines: Vec<UpdateLine>,
    pub selection: Vec<Selected>,
    pub build: BuildPlan,
}

/// What [`AgentUpdater::run`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateReport {
    /// A new merge request `iid`.
    Opened { iid: i64 },
    /// The same update's merge request `iid` is open already.
    AlreadyOpen { iid: i64 },
    /// Nothing to do, and why (`no_lock`, `up_to_date`, `not_in_lock`,
    /// `project_gone`).
    Skipped(&'static str),
}

pub(crate) fn not_needed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::AgentUpdateNotNeeded).with_context("reason", reason)
}

fn not_opened(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::AgentUpdateNotOpened).with_context("reason", reason)
}

fn reason_of(e: &ForgeError) -> Option<&'static str> {
    let reason = e
        .context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.as_str())?;
    ["no_lock", "up_to_date", "not_in_lock"]
        .into_iter()
        .find(|r| *r == reason)
}

/// The update's selection from `lock` (module docs). `published`: the
/// locked `(slug, version)` pairs still published (not revoked).
/// `Err`: forge-agent's problems (`FG-AGENT-0008`).
pub fn update_selection(
    lock: &Lock,
    catalog: &[CatalogVersion],
    published: &BTreeSet<(String, String)>,
    adapter: Option<&str>,
) -> Result<(Vec<Selected>, Vec<UpdateLine>), Vec<forge_agent::Problem>> {
    let locked_adapter = lock
        .parts
        .iter()
        .find(|p| p.kind == "adapter")
        .map(|p| p.slug.as_str());
    let base = adapter.or(locked_adapter).unwrap_or(DEFAULT_ADAPTER);
    let current: Vec<String> = lock
        .parts
        .iter()
        .filter(|p| p.kind != "adapter")
        .map(|p| p.slug.clone()) // clone: recommend takes owned slugs
        .collect();
    let found = recommend(catalog, base, &current)?;
    let mut picked: BTreeMap<String, Selected> = found
        .parts
        .into_iter()
        .map(|r| {
            (
                r.slug.clone(), // clone: the map's key and the selection's
                Selected {
                    slug: r.slug,
                    version: r.version,
                    selected_by: r.selected_by.to_owned(),
                    reason: Some(r.reason),
                },
            )
        })
        .collect();
    let usable =
        |slug: &str, version: &str| published.contains(&(slug.to_owned(), version.to_owned()));
    for p in &lock.parts {
        // The base model the generation migration leaves is not kept.
        if p.kind == "adapter" && p.slug != base {
            continue;
        }
        let kept = Selected {
            slug: p.slug.clone(),       // clone: the selection owns it
            version: p.version.clone(), // clone: as above
            selected_by: "rule".to_owned(),
            reason: Some("kept".to_owned()),
        };
        match picked.get_mut(&p.slug) {
            // Never down: a newer locked version that is still published.
            Some(sel) => {
                let newer = match (Version::parse(&p.version), Version::parse(&sel.version)) {
                    (Some(l), Some(n)) => l > n,
                    _ => false,
                };
                if newer && usable(&p.slug, &p.version) {
                    *sel = kept;
                }
            }
            // No stable version: the locked one stays while published.
            None if usable(&p.slug, &p.version) => {
                picked.insert(p.slug.clone(), kept); // clone: the map's key
            }
            None => {}
        }
    }
    let mut lines: Vec<UpdateLine> = lock
        .parts
        .iter()
        .map(|p| UpdateLine {
            slug: p.slug.clone(),            // clone: the line owns it
            kind: p.kind.clone(),            // clone: as above
            locked: Some(p.version.clone()), // clone: as above
            locked_revoked: !usable(&p.slug, &p.version),
            next: picked.get(&p.slug).map(|s| s.version.clone()), // clone: as above
        })
        .collect();
    for (slug, sel) in &picked {
        if !lock.parts.iter().any(|p| &p.slug == slug) {
            let kind = catalog
                .iter()
                .find(|c| &c.slug == slug)
                .map_or_else(|| "unknown".to_owned(), |c| c.kind.clone()); // clone: the line owns it
            lines.push(UpdateLine {
                slug: slug.clone(), // clone: as above
                kind,
                locked: None,
                locked_revoked: false,
                next: Some(sel.version.clone()), // clone: as above
            });
        }
    }
    lines.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok((picked.into_values().collect(), lines))
}

impl Marketplace {
    /// The update of `project`'s configuration (module docs): the lines,
    /// the selection and the A7 plan. `FG-AGENT-0015` when there is
    /// nothing to update; the builder's own errors otherwise.
    pub async fn plan_update(
        &self,
        project: ProjectId,
        repo: RepoId,
        adapter: Option<&str>,
    ) -> Result<UpdatePlan, ForgeError> {
        bounded(
            "agent_update",
            self.plan_update_inner(project, repo, adapter),
        )
        .await
    }

    async fn plan_update_inner(
        &self,
        project: ProjectId,
        repo: RepoId,
        adapter: Option<&str>,
    ) -> Result<UpdatePlan, ForgeError> {
        let tip = match self.default_tip(repo).await {
            Ok((_, tip)) => tip,
            // An empty repository has no lock.
            Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => {
                return Err(not_needed("no_lock"))
            }
            Err(e) => return Err(e),
        };
        let managed = self.managed_at(repo, &tip).await?;
        let Some(bytes) = managed.lock.as_deref() else {
            return Err(not_needed("no_lock"));
        };
        let lock = Lock::parse(bytes).map_err(|p| problems_error(&[p]))?;
        let pairs: Vec<(String, String)> = lock
            .parts
            .iter()
            .map(|p| (p.slug.clone(), p.version.clone())) // clone: the query owns them
            .collect();
        let published: BTreeSet<(String, String)> = builds::versions(&self.db, &pairs)
            .await?
            .into_iter()
            .filter(|v| v.state == "published")
            .map(|v| (v.slug, v.version))
            .collect();
        let catalog = self.stable_catalog().await?;
        let (selection, lines) = update_selection(&lock, &catalog, &published, adapter)
            .map_err(|p| problems_error(&p))?;
        let now: BTreeSet<(&str, &str)> = lock
            .parts
            .iter()
            .map(|p| (p.slug.as_str(), p.version.as_str()))
            .collect();
        let next: BTreeSet<(&str, &str)> = selection
            .iter()
            .map(|s| (s.slug.as_str(), s.version.as_str()))
            .collect();
        if now == next {
            return Err(not_needed("up_to_date"));
        }
        let build = self.plan_inner(project, repo, &selection).await?;
        if build.writes.is_empty() && build.removes.is_empty() {
            return Err(not_needed("up_to_date"));
        }
        Ok(UpdatePlan {
            lines,
            selection,
            build,
        })
    }
}

impl Marketplace {
    /// The parts of the lock at `repo`'s default branch's tip whose locked
    /// version is no longer published (revoked), as `slug@version`;
    /// `None`: no lock there (or an empty repository). Read only, bounded
    /// like the plan (`forge doctor`'s `agent.revoked_in_use`, plan 16 §8).
    pub async fn revoked_at_tip(&self, repo: RepoId) -> Result<Option<Vec<String>>, ForgeError> {
        bounded("agent_revoked_at_tip", self.revoked_at_tip_inner(repo)).await
    }

    async fn revoked_at_tip_inner(&self, repo: RepoId) -> Result<Option<Vec<String>>, ForgeError> {
        let tip = match self.default_tip(repo).await {
            Ok((_, tip)) => tip,
            Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => return Ok(None),
            Err(e) => return Err(e),
        };
        let managed = self.managed_at(repo, &tip).await?;
        let Some(bytes) = managed.lock.as_deref() else {
            return Ok(None);
        };
        let lock = Lock::parse(bytes).map_err(|p| problems_error(&[p]))?;
        let pairs: Vec<(String, String)> = lock
            .parts
            .iter()
            .map(|p| (p.slug.clone(), p.version.clone())) // clone: the query owns them
            .collect();
        let published: BTreeSet<(String, String)> = builds::versions(&self.db, &pairs)
            .await?
            .into_iter()
            .filter(|v| v.state == "published")
            .map(|v| (v.slug, v.version))
            .collect();
        Ok(Some(
            pairs
                .into_iter()
                .filter(|pair| !published.contains(pair))
                .map(|(slug, version)| format!("{slug}@{version}"))
                .collect(),
        ))
    }
}

impl Marketplace {
    /// The merge request of the same update (the branch of `plan`'s lock
    /// digest into its target), the newest: its state and iid.
    pub async fn existing_update(
        &self,
        project: ProjectId,
        plan: &UpdatePlan,
    ) -> Result<Option<(MergeRequestState, i64)>, ForgeError> {
        let branch = builder_branch(&plan.build.compiled.lock_digest)?;
        Ok(
            review::latest_between(&self.db, project, (&branch, &plan.build.target))
                .await?
                .map(|mr| (mr.state, mr.iid)),
        )
    }

    /// Queue `user`'s update of `project` (the route's 202, after
    /// [`Marketplace::plan_update`] found one): the job id, `None` when the
    /// same update (the same lock digest, asked by the same person) is
    /// queued or done already. The job plans again when it runs.
    pub async fn ask_update(
        &self,
        project: ProjectId,
        user: UserId,
        adapter: Option<&str>,
        plan: &UpdatePlan,
        request_id: &RequestId,
    ) -> Result<Option<i64>, ForgeError> {
        let digest = plan
            .build
            .compiled
            .lock_digest
            .strip_prefix("sha256:")
            .unwrap_or(&plan.build.compiled.lock_digest);
        let job = builds::agent_update_job(
            &AgentUpdateRequest {
                project,
                origin: AgentUpdateOrigin::Update,
                requested_by: Some(user),
                adapter: adapter.map(str::to_owned),
                revoked: None,
            },
            &format!("agent_update:{}:{}:{digest}", project.get(), user.get()),
            request_id.clone(), // clone: the job keeps the request's id
        )?;
        Ok(match builds::enqueue_update(&self.db, &job).await? {
            forge_db::jobs::Enqueued::Queued(id) => Some(id.get()),
            forge_db::jobs::Enqueued::Deduped => None,
        })
    }
}

/// The future of [`UpdateOpener::open`].
pub type OpenFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(MergeRequestId, i64), ForgeError>> + Send + 'a>>;

/// Opens the update's merge request with the person's proof (decided again
/// inside the open's transaction): forge-review's `Review::open`, wired by
/// `forge serve` — this crate never opens a merge request itself.
pub trait UpdateOpener: Send + Sync {
    /// `(id, iid)` of the merge request opened from the proof's branch.
    fn open<'a>(
        &'a self,
        request_id: &'a RequestId,
        proof: &'a Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
        title: String,
        description: String,
    ) -> OpenFuture<'a>;
}

/// Opens the update merge requests (module docs): the
/// `agent.update_merge_request` job's work.
#[derive(Clone)]
pub struct AgentUpdater {
    market: Marketplace,
    git: Git,
    storage: StorageRoot,
    opener: Arc<dyn UpdateOpener>,
}

impl std::fmt::Debug for AgentUpdater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentUpdater").finish_non_exhaustive()
    }
}

/// The person's `MergeRequestCreate`, decided before git writes anything
/// reachable (with `AgentBuilderCompile` and the branch's push).
struct Decided {
    create: Authorized<MergeRequestCtx>,
}

impl AgentUpdater {
    pub fn new(
        market: Marketplace,
        git: Git,
        storage: StorageRoot,
        opener: Arc<dyn UpdateOpener>,
    ) -> Self {
        Self {
            market,
            git,
            storage,
            opener,
        }
    }

    /// Run one update (module docs); logged once as
    /// `forge.agent.update_opened` / `forge.agent.update_skipped`. Errors
    /// (the job retries or dies with the code, logged once by the job
    /// runner): `FG-AGENT-0016`, the builder's codes, git, storage and the
    /// database.
    pub async fn run(
        &self,
        asked: &AgentUpdateRequest,
        request_id: &RequestId,
    ) -> Result<UpdateReport, ForgeError> {
        let revoked = asked
            .revoked
            .as_ref()
            .map(|(s, v)| format!("{s}@{v}"))
            .unwrap_or_default();
        let ran = self.run_inner(asked, request_id).await.map_err(|e| {
            e.with_context("project_id", asked.project.get())
                .with_context("origin", asked.origin.as_str())
                .with_context("revoked", revoked.clone()) // clone: the log line below keeps it
        })?;
        let (outcome, reason, iid) = match &ran {
            UpdateReport::Opened { iid } => ("opened", "", *iid),
            UpdateReport::AlreadyOpen { iid } => ("already_open", "", *iid),
            UpdateReport::Skipped(r) => ("skipped", *r, 0),
        };
        if iid > 0 {
            tracing::info!(
                target: "forge_marketplace",
                event = "forge.agent.update_opened",
                request_id = %request_id,
                project_id = asked.project.get(),
                origin = asked.origin.as_str(),
                revoked = %revoked,
                outcome,
                iid,
                "agent configuration update"
            );
        } else if asked.origin == AgentUpdateOrigin::Revocation && reason != "not_in_lock" {
            // Review A9 (silent-failure MEDIUM): a revocation's job that
            // opened nothing although the lock may still use the version.
            tracing::warn!(
                target: "forge_marketplace",
                event = "forge.agent.update_skipped",
                request_id = %request_id,
                project_id = asked.project.get(),
                origin = asked.origin.as_str(),
                revoked = %revoked,
                reason,
                "a revocation's update opened nothing"
            );
        } else {
            tracing::info!(
                target: "forge_marketplace",
                event = "forge.agent.update_skipped",
                request_id = %request_id,
                project_id = asked.project.get(),
                origin = asked.origin.as_str(),
                revoked = %revoked,
                reason,
                "agent configuration update: nothing to do"
            );
        }
        Ok(ran)
    }

    async fn run_inner(
        &self,
        asked: &AgentUpdateRequest,
        request_id: &RequestId,
    ) -> Result<UpdateReport, ForgeError> {
        let db = &self.market.db;
        // Who it may be opened for: the person who asked, or for a
        // revocation the project's newest builders (review A9: the newest
        // one may have left; the next one who may is taken).
        let candidates: Vec<UserId> = match (asked.origin, asked.requested_by) {
            (AgentUpdateOrigin::Update, Some(user)) => vec![user],
            (AgentUpdateOrigin::Update, None) => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "update_without_requester"))
            }
            (AgentUpdateOrigin::Revocation, _) => {
                builds::builders(db, asked.project, MAX_CANDIDATES).await?
            }
        };
        let Some(first) = candidates.first().copied() else {
            return Err(not_opened("no_author"));
        };
        let Some(record) = forge_db::resolve::load_project(db, asked.project, Some(first)).await?
        else {
            return Ok(UpdateReport::Skipped("project_gone"));
        };
        if record.repository.state != RepositoryState::Ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("state", record.repository.state.as_str()));
        }
        let repo = record.repository.id;
        let plan = match self
            .market
            .plan_update(asked.project, repo, asked.adapter.as_deref())
            .await
        {
            Ok(plan) => plan,
            Err(e) if e.code() == ErrorCode::AgentUpdateNotNeeded => {
                return Ok(UpdateReport::Skipped(reason_of(&e).unwrap_or("up_to_date")));
            }
            Err(e) => return Err(e),
        };
        // A revocation's update only where the lock still uses it.
        if let Some((slug, version)) = &asked.revoked {
            let uses = plan
                .lines
                .iter()
                .any(|l| &l.slug == slug && l.locked.as_ref() == Some(version));
            if !uses {
                return Ok(UpdateReport::Skipped("not_in_lock"));
            }
        }
        let branch = builder_branch(&plan.build.compiled.lock_digest)?;
        let target = plan.build.target.clone(); // clone: the merge request owns it
                                                // The same update, asked before.
        if let Some(mr) = review::latest_between(db, asked.project, (&branch, &target)).await? {
            match mr.state {
                MergeRequestState::Open => {
                    // Idempotent records: a run that ended after the open.
                    self.record_build(asked, mr.author, &plan, mr.id, request_id)
                        .await?;
                    self.record_event(asked, mr.id, mr.iid, mr.author, &plan, request_id)
                        .await?;
                    return Ok(UpdateReport::AlreadyOpen { iid: mr.iid });
                }
                MergeRequestState::Closed | MergeRequestState::Merged => {
                    return Err(not_opened("closed_by_person").with_context("iid", mr.iid))
                }
            }
        }
        let (author, decided) = self
            .first_allowed(&candidates, asked.project, (&branch, &target))
            .await?;
        let (commit, created) = self.write_branch(repo, &branch, &plan.build).await?;
        let ctx = system_ctx(request_id);
        let opened = match self
            .opener
            .open(
                request_id,
                &decided.create,
                record.repository,
                title_of(asked),
                description_of(asked, &plan),
            )
            .await
        {
            Ok(opened) => opened,
            Err(e) => {
                // The merge request may have committed although the answer
                // was lost: look before putting anything back (a failure of
                // the look keeps the open's error, logged once here).
                match review::open_iid_between(db, asked.project, (&branch, &target)).await {
                    Ok(Some(iid)) => return Ok(UpdateReport::AlreadyOpen { iid }),
                    Ok(None) => {
                        if created {
                            self.undo(repo, &branch, &commit, request_id).await;
                        }
                    }
                    Err(look) => telemetry::log_failure(
                        &look
                            .with_context("phase", "agent_update_open_lookup")
                            .with_context("project_id", asked.project.get()),
                        request_id,
                    ),
                }
                return Err(e.with_context("phase", "open_merge_request"));
            }
        };
        // The branch's push is recorded by the run that opened the merge
        // request (also when an interrupted run had written the branch).
        self.record_branch(&ctx, repo, &branch, &commit, request_id)
            .await;
        self.record_build(asked, author, &plan, opened.0, request_id)
            .await?;
        self.record_event(asked, opened.0, opened.1, author, &plan, request_id)
            .await?;
        Ok(UpdateReport::Opened { iid: opened.1 })
    }

    /// The first of `candidates` allowed to have the update opened
    /// ([`AgentUpdater::decide`] on their current role); none is
    /// `FG-AGENT-0016 author_not_allowed` with the first refusal.
    async fn first_allowed(
        &self,
        candidates: &[UserId],
        project: ProjectId,
        names: (&RefName, &RefName),
    ) -> Result<(UserId, Decided), ForgeError> {
        let mut first_refusal = None;
        for author in candidates {
            let Some(record) =
                forge_db::resolve::load_project(&self.market.db, project, Some(*author)).await?
            else {
                continue;
            };
            let loaded = LoadedProject {
                id: record.id,
                visibility: record.access.visibility,
                role: record.access.role,
                pending_deletion: record.access.pending_deletion,
            };
            match self.decide(*author, loaded, names, project).await {
                Ok(decided) => return Ok((*author, decided)),
                Err(e) if e.code() == ErrorCode::AgentUpdateNotOpened => {
                    first_refusal.get_or_insert(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(first_refusal
            .unwrap_or_else(|| not_opened("author_not_allowed"))
            .with_context("candidates", candidates.len()))
    }

    /// `AgentBuilderCompile`, the branch's create as a push and
    /// `MergeRequestCreate` for `author` with their current rights
    /// (`FG-AGENT-0016 author_not_allowed` with the refusal's code).
    async fn decide(
        &self,
        author: UserId,
        project: LoadedProject,
        (branch, target): (&RefName, &RefName),
        project_id: ProjectId,
    ) -> Result<Decided, ForgeError> {
        let db = &self.market.db;
        let refused = |e: ForgeError| {
            not_opened("author_not_allowed")
                .with_context("author_id", author.get())
                .with_context("denied_code", e.code().id())
        };
        let Some((kind, active, is_admin)) = review::reverts::account_facts(db, author).await?
        else {
            return Err(not_opened("author_not_allowed")
                .with_context("author_id", author.get())
                .with_context("denied_code", "no_account"));
        };
        let principal =
            Principal::authenticated(author, is_admin, active, Credential::Session, kind);
        let _builder = prove(
            &principal,
            Action::AgentBuilderCompile,
            ProjectCtx::new(project),
        )
        .map_err(refused)?;
        let push =
            prove(&principal, Action::RepoPush, ProjectCtx::new(project)).map_err(refused)?;
        let rules = forge_db::protected_refs::rules_for(db, project_id).await?;
        let ref_ctx = RefUpdateCtx::new(branch.clone(), RefUpdate::Create, &rules); // clone: the context owns its name
        authorize_ref_updates(&push, std::slice::from_ref(&ref_ctx))?
            .into_iter()
            .collect::<Result<Vec<()>, ForgeError>>()
            .map_err(refused)?;
        let create = prove(
            &principal,
            Action::MergeRequestCreate,
            MergeRequestCtx::new(
                project,
                LoadedMergeRequest {
                    iid: None,
                    author,
                    state: MergeRequestState::Open,
                    draft: false,
                    source_branch: branch.clone(), // clone: the context owns its names
                    target_branch: target.clone(), // clone: as above
                },
            ),
        )
        .map_err(refused)?;
        Ok(Decided { create })
    }

    /// The bot's commit on the new branch, or the branch an interrupted run
    /// left with exactly this build: `(tip, created)`.
    async fn write_branch(
        &self,
        repo: RepoId,
        branch: &RefName,
        build: &BuildPlan,
    ) -> Result<(ObjectId, bool), ForgeError> {
        if let Some(tip) = self
            .git
            .read_ref(&self.storage, repo, branch, UPDATE_GIT_TIMEOUT)
            .await?
        {
            let at_tip = self
                .market
                .browse
                .agent_config(repo, tip.as_str(), MAX_TIP_FILES, MAX_TIP_BYTES)
                .await?;
            let same = at_tip.lock.as_deref() == Some(build.compiled.lock.as_slice())
                && build
                    .compiled
                    .files
                    .iter()
                    .all(|(p, b)| at_tip.files.get(p) == Some(b));
            if same {
                return Ok((tip, false));
            }
            return Err(ForgeError::new(ErrorCode::FileCommitConflict)
                .with_context("reason", "branch_exists")
                .with_context("branch", branch.as_str().to_owned()));
        }
        // The secret rules on each written file (a published part never
        // brings a credential into a repository unseen).
        for (path, bytes) in &build.writes {
            if let SecretScan::Found(rule) = secret_scan::scan(bytes) {
                return Err(ForgeError::new(ErrorCode::PushSecretDetected)
                    .with_context("verdict", "refused")
                    .with_context("first_rule", rule)
                    .with_context("first_path", path.clone())); // clone: the error owns it
            }
        }
        let writes: Vec<(FilePath, &[u8])> = build
            .writes
            .iter()
            .map(|(p, b)| FilePath::parse(p).map(|p| (p, b.as_slice())))
            .collect::<Result<_, _>>()?;
        let removes: Vec<FilePath> = build
            .removes
            .iter()
            .map(|p| FilePath::parse(p))
            .collect::<Result<_, _>>()?;
        let bot = CommitIdentity::new(BUILDER_NAME, BUILDER_EMAIL, BUILDER_NAME)?;
        let message = format!(
            "chore(agent): update the agent configuration\n\nGenerated by the GitStation agent builder from {} signed part version(s).\nLock: {}\n",
            build.parts.len(),
            build.compiled.lock_digest
        );
        let written = tokio::time::timeout(
            COMMIT_DEADLINE,
            self.git.commit_tree_change(
                &self.storage,
                repo,
                &TreeChange {
                    parent: &build.base,
                    branch,
                    writes: &writes,
                    removes: &removes,
                    message: &message,
                    author: &bot,
                    committer: &bot,
                },
                UPDATE_GIT_TIMEOUT,
            ),
        )
        .await;
        self.market.browse.invalidate(repo);
        match written {
            Ok(commit) => Ok((commit?, true)),
            Err(_) => Err(ForgeError::new(ErrorCode::GitTimeout)
                .with_context("operation", "agent_update_commit")
                .with_context("timeout_ms", COMMIT_DEADLINE.as_millis())),
        }
    }

    /// Delete the new branch (the merge request could not be opened): a
    /// compare-and-swap on the commit. A failure is logged once; the caller
    /// returns the original error.
    async fn undo(
        &self,
        repo: RepoId,
        branch: &RefName,
        commit: &ObjectId,
        request_id: &RequestId,
    ) {
        let undone = self
            .git
            .update_refs(
                &self.storage,
                repo,
                &[RefChange::Delete {
                    name: branch.clone(), // clone: the change owns its name
                    old: commit.clone(),  // clone: as above
                }],
                UPDATE_GIT_TIMEOUT,
            )
            .await;
        self.market.browse.invalidate(repo);
        if let Err(e) = undone {
            telemetry::log_failure(
                &e.with_context("phase", "agent_update_branch_undo")
                    .with_context("ref", branch.as_str().to_owned()),
                request_id,
            );
        }
    }

    /// The branch's create as a push (`repository.pushed`). The branch
    /// stands whatever happens here: a failure is logged once.
    async fn record_branch(
        &self,
        ctx: &ChangeContext,
        repo: RepoId,
        branch: &RefName,
        commit: &ObjectId,
        request_id: &RequestId,
    ) {
        let zero = match ObjectId::parse(&"0".repeat(commit.as_str().len())) {
            Ok(zero) => zero,
            Err(e) => {
                telemetry::log_failure(&e.with_context("phase", "agent_update_push"), request_id);
                return;
            }
        };
        let record = PushRecord {
            repository_id: repo,
            updates: vec![PushedRef {
                name: branch.clone(), // clone: the record owns its name
                old: zero,
                new: commit.clone(), // clone: as above
                kind: PushUpdateKind::Create,
                commits: vec![commit.clone()], // clone: as above
                total_commits: 1,
                total_commits_partial: false,
            }],
        };
        let commits = PushedCommits {
            commits: vec![commit.clone()], // clone: the record owns it
            complete: true,
        };
        if let Err(e) = forge_db::repositories::record_push_with_commits(
            &self.market.db,
            ctx,
            &record,
            &commits,
        )
        .await
        {
            telemetry::log_failure(
                &e.with_context("phase", "agent_update_push")
                    .with_context("repo_id", repo.get())
                    .with_context("ref", branch.as_str().to_owned()),
                request_id,
            );
        }
    }

    /// The build's record, once per merge request (review A9: the record
    /// is what a later revocation finds the project by, so a failure is the
    /// job's — retried, the next run finds the merge request open).
    async fn record_build(
        &self,
        asked: &AgentUpdateRequest,
        author: UserId,
        plan: &UpdatePlan,
        merge_request: MergeRequestId,
        request_id: &RequestId,
    ) -> Result<(), ForgeError> {
        builds::record_build_once(
            &self.market.db,
            &NewBuild {
                project: asked.project,
                requested_by: author,
                adapter_version_id: plan.build.adapter_version_id,
                selection: &plan.build.selection,
                dry_run: false,
                request_id: request_id.as_str(),
                origin: asked.origin.as_str(),
            },
            &plan.build.compiled.lock_digest,
            merge_request,
        )
        .await
        .map(drop)
        .map_err(|e| e.with_context("phase", "agent_update_record_build"))
    }

    /// `merge_request.agent_update_opened` (deduplicated by the merge
    /// request: a job run again records it once). A failure is the job's
    /// (retried: the next run finds the merge request open and records it).
    async fn record_event(
        &self,
        asked: &AgentUpdateRequest,
        merge_request: MergeRequestId,
        iid: i64,
        author: UserId,
        plan: &UpdatePlan,
        request_id: &RequestId,
    ) -> Result<(), ForgeError> {
        let change = MergeRequestChange::AgentUpdateOpened {
            merge_request,
            iid,
            author,
            origin: asked.origin,
            lock_digest: plan.build.compiled.lock_digest.clone(), // clone: the record owns it
            revoked: asked.revoked.as_ref().map(|(s, v)| format!("{s}@{v}")),
        };
        builds::record_agent_update_opened(
            &self.market.db,
            &system_ctx(request_id),
            asked.project,
            change,
        )
        .await
        .map(drop)
        .map_err(|e| e.with_context("phase", "agent_update_event"))
    }
}

fn title_of(asked: &AgentUpdateRequest) -> String {
    match &asked.revoked {
        Some((slug, version)) => {
            format!("Replace the revoked agent part {slug}@{version}")
        }
        None if asked.adapter.is_some() => {
            "Move the agent configuration to a new base model".to_owned()
        }
        None => "Update the agent configuration".to_owned(),
    }
}

fn description_of(asked: &AgentUpdateRequest, plan: &UpdatePlan) -> String {
    let mut text = String::from(match asked.origin {
        AgentUpdateOrigin::Revocation => {
            "A part version this project's agent configuration uses was revoked. The agent builder opened this update from signed marketplace parts.\n\n"
        }
        AgentUpdateOrigin::Update => {
            "The agent builder opened this update from signed marketplace parts.\n\n"
        }
    });
    text.push_str("| part | locked | next |\n|---|---|---|\n");
    for line in &plan.lines {
        let locked = match (&line.locked, line.locked_revoked) {
            (Some(v), true) => format!("{v} (revoked)"),
            (Some(v), false) => v.clone(), // clone: the text owns it
            (None, _) => "—".to_owned(),
        };
        let next = line.next.as_deref().unwrap_or("removed");
        text.push_str(&format!("| {} | {locked} | {next} |\n", line.slug));
    }
    text.push_str(
        "\n`forge/agent-lock` verifies the configuration; a Maintainer who is not the author approves it before it merges. Runbook: agent-supply-chain.",
    );
    text
}

fn system_ctx(request_id: &RequestId) -> ChangeContext {
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    ctx.actor_label = Some(UPDATE_ACTOR_LABEL.to_owned());
    ctx
}

#[cfg(test)]
#[path = "updates_tests.rs"]
mod tests;
