//! forge-checks: check runs (M3 C1, ADR-0024 §10, §11; plan 02 §5.4). A
//! feature crate beside forge-review: it validates a report, decides who
//! may report it through forge-auth's `authorize()` and records it through
//! forge-db. forge-review never depends on it (CLAUDE.md §3-9): the merge
//! conditions read the runs through `forge_core::CheckResultsSource`.
//!
//! **Every write is decided twice** (as forge-hosting and forge-review):
//! the route proves `CheckWrite` on what its loader read (the project, the
//! commit and name of the report, and whether the caller holds a grant of
//! the name's namespace) and hands the `Authorized<CheckRunCtx>` here;
//! [`Checks::report`] validates the body, checks the commit is reachable
//! from a branch or tag of the repository ([`CommitProbe`]: an unreachable
//! commit is answered exactly like a missing one, and a check that cannot
//! decide in time writes nothing), then calls forge-db's
//! `report_check_run_decided`, which locks the project, the caller's roles,
//! account and grant and calls `decide` — here `Authorized::reconfirm` on
//! those facts. A grant revoked or a membership removed in between is seen.
//! Only this crate calls those commands (`tests/decided_callers.rs`).

use std::sync::Arc;

use forge_auth::{
    AccountFacts, Action, Authorized, CheckNamespaceGrantCtx, CheckRunCtx, LoadedProject,
    MergeRequestCtx, Principal, ProjectCtx, ServiceAccountView,
};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    CheckName, CheckNamespace, ObjectId, ProjectId, ProjectRepository, RepoId, RepositoryState,
    RequestId, UserId,
};
use forge_db::access::Account;
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::{self, ReportCredential};
use forge_db::projects::ProjectAccessFacts;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

pub mod ci_lint;
pub mod confidential;
pub mod doc_lint;
pub mod doc_migration;
pub mod risk;
pub mod spec_checks;
pub mod spec_drift;
pub mod spec_index;
pub mod spec_refs;
pub mod validate;

pub use doc_migration::DocLintMigrations;
pub use forge_db::checks::doc_lint_migration::{
    DocLintMigration, MigrationRead, SetOutcome as MigrationSetOutcome,
};
pub use forge_db::checks::{
    CheckAnnotation, Grant, GrantOutcome, NewCheckRun, Reported, StoredCheckRun,
};
pub use validate::{parse_name, AnnotationInput, ReportInput};

/// The commit a full id names, if a branch or tag of the repository
/// reaches it (M3 P6's reachability check). `Ok(commit)` when it does;
/// `FG-REPO-0023 reason=ref_not_found` when it does not, does not exist or
/// the check could not decide (its time limit, a full slot table) — the
/// same answer for each, so the status never reveals existence; another
/// error otherwise. Never "reachable" on a failure.
pub trait CommitProbe: Send + Sync {
    /// `caller` is the caller's key (`user:<id>`) for the browse limits.
    fn reachable_commit<'a>(
        &'a self,
        repo: RepoId,
        sha: &'a ObjectId,
        caller: &'a str,
    ) -> LookupFuture<'a, ObjectId>;
}

/// The browse API's id lookup (`resolve_ref` on a full id: reachable from
/// a branch or a tag only).
impl CommitProbe for forge_git::browse::Browser {
    fn reachable_commit<'a>(
        &'a self,
        repo: RepoId,
        sha: &'a ObjectId,
        caller: &'a str,
    ) -> LookupFuture<'a, ObjectId> {
        Box::pin(async move {
            let resolved = self.resolve_ref(repo, sha.as_str(), caller).await?;
            ObjectId::parse(&resolved.commit).map_err(|_| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "resolved_commit_invalid")
                    .with_context("repo_id", repo.get())
            })
        })
    }
}

/// The request a write belongs to (the audit row's request id and token).
#[derive(Debug, Clone)]
pub struct ChecksRequest {
    pub request_id: RequestId,
    /// The personal access token the request authenticated with (`None`
    /// for a session).
    pub credential_id: Option<i64>,
}

/// The check run commands and reads on the database.
#[derive(Clone)]
pub struct Checks {
    db: Db,
    commits: Arc<dyn CommitProbe>,
}

impl std::fmt::Debug for Checks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checks").finish_non_exhaustive()
    }
}

/// `FG-SYS-0001 reason=proof_action_mismatch` unless the proof is for
/// `expected`.
fn require(action: Action, expected: Action) -> Result<(), ForgeError> {
    if action == expected {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "proof_action_mismatch")
        .with_context("command_action", expected.name())
        .with_context("proof_action", action.name()))
}

fn caller(principal: &Principal, action: Action) -> Result<UserId, ForgeError> {
    principal.user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_without_user")
            .with_context("action", action.name())
    })
}

fn change_context(request: &ChecksRequest, principal: &Principal, user: UserId) -> ChangeContext {
    let actor = match principal.account_kind() {
        forge_auth::AccountKind::ServiceAccount => Actor::ServiceAccount(user),
        forge_auth::AccountKind::Human | forge_auth::AccountKind::Bot => Actor::User(user),
    };
    let mut ctx = ChangeContext::new(actor, request.request_id.clone());
    ctx.credential_id = request.credential_id;
    // An act-as principal never reaches these commands (ADR-0025 §4: no
    // allowed action of theirs); its token's scopes all the same.
    ctx.effective_scope = principal.scope_names();
    ctx
}

fn account(a: Account) -> AccountFacts {
    AccountFacts {
        active: a.active,
        is_admin: a.is_admin,
    }
}

fn loaded(facts: &ProjectAccessFacts) -> LoadedProject {
    LoadedProject {
        id: facts.project,
        visibility: facts.access.visibility,
        role: facts.access.role,
        pending_deletion: facts.access.pending_deletion,
    }
}

/// The repository must exist on disk to look a commit up (`FG-REPO-0016`).
fn ready(project: ProjectId, repo: ProjectRepository) -> Result<RepoId, ForgeError> {
    if repo.state == RepositoryState::Ready {
        return Ok(repo.id);
    }
    Err(ForgeError::new(ErrorCode::RepositoryNotReady)
        .with_context("repo_id", repo.id.get())
        .with_context("project_id", project.get())
        .with_context("state", repo.state.as_str()))
}

impl Checks {
    pub fn new(db: Db, commits: Arc<dyn CommitProbe>) -> Self {
        Self { db, commits }
    }

    /// The document lint's migration setting on the same database (plan
    /// 13 Q13).
    pub fn doc_lint_migrations(&self) -> DocLintMigrations {
        DocLintMigrations::new(self.db.clone()) // clone: pool handles
    }

    /// Whether `user` holds a grant of `namespace` on `project` now (the
    /// report route's loader; the write reads it again under the lock). A
    /// grant is for one project (review fix 1).
    pub async fn granted(
        &self,
        user: UserId,
        project: ProjectId,
        namespace: CheckNamespace,
    ) -> Result<bool, ForgeError> {
        checks::granted(&self.db, user, project, namespace).await
    }

    /// The commit `sha` of `repo`, if a branch or tag reaches it
    /// ([`CommitProbe`]).
    async fn reachable(
        &self,
        project: ProjectId,
        repo: ProjectRepository,
        sha: &ObjectId,
        user: UserId,
    ) -> Result<ObjectId, ForgeError> {
        let id = ready(project, repo)?;
        let key = format!("user:{}", user.get());
        self.commits.reachable_commit(id, sha, &key).await
    }

    /// `POST …/commits/{sha}/checks` (`CheckWrite`): record `input` under
    /// the proof's name on the proof's commit. The body's rules are checked
    /// after the decision (`FG-REVIEW-0020`), then the commit's
    /// reachability (`FG-REPO-0023` like a missing commit; nothing is
    /// written when it cannot be decided), then the write decides again
    /// under the locks.
    pub async fn report(
        &self,
        request: &ChecksRequest,
        proof: &Authorized<CheckRunCtx>,
        repo: ProjectRepository,
        input: &ReportInput,
    ) -> Result<Reported, ForgeError> {
        require(proof.action(), Action::CheckWrite)?;
        let user = caller(proof.principal(), Action::CheckWrite)?;
        let (project, head, name) = proof.id().clone(); // clone: the command owns its target
        let new = validate::validate(name.clone(), input) // clone: the reconfirm needs it too
            .map_err(|e| {
                e.with_context("project_id", project.get())
                    .with_context("name", name.as_str().to_owned())
            })?;
        self.reachable(project, repo, &head, user).await?;
        let ctx = change_context(request, proof.principal(), user);
        let (reported, ()) = checks::report_check_run_decided(
            &self.db,
            &ctx,
            (project, &head),
            user,
            ReportCredential(request.credential_id),
            &new,
            |facts| {
                let fresh = CheckRunCtx::new(
                    loaded(&facts.project),
                    head.clone(), // clone: the fresh context owns its id
                    name.clone(), // clone: as above
                    facts.granted,
                );
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        Ok(reported)
    }

    /// `GET …/commits/{sha}/checks` (`CheckRead`): every run on `sha` of
    /// the proof's project, by name, if a branch or tag reaches it (an
    /// unreachable commit is not found, like a missing one).
    pub async fn list(
        &self,
        proof: &Authorized<ProjectCtx>,
        repo: ProjectRepository,
        sha: &ObjectId,
    ) -> Result<Vec<StoredCheckRun>, ForgeError> {
        require(proof.action(), Action::CheckRead)?;
        let user = caller(proof.principal(), Action::CheckRead)?;
        let project = *proof.id();
        let commit = self.reachable(project, repo, sha, user).await?;
        checks::check_runs(&self.db, project, &commit).await
    }

    /// Every run on `head` — the current head of the merge request the
    /// proof is for — of its project, by name (M3 X5: the merge request's
    /// context, read like the merge request itself). The head is the
    /// stored one (a version ref keeps it), so no branch reachability is
    /// asked.
    pub async fn merge_request_head_runs(
        &self,
        proof: &Authorized<MergeRequestCtx>,
        head: &ObjectId,
    ) -> Result<Vec<StoredCheckRun>, ForgeError> {
        require(proof.action(), Action::MergeRequestRead)?;
        caller(proof.principal(), Action::MergeRequestRead)?;
        let (project, _) = *proof.id();
        checks::check_runs(&self.db, project, head).await
    }

    /// `PUT` (`grant = true`) / `DELETE …/check-grants/{project_id}/{namespace}`
    /// (`CheckNamespaceGrantManage`): decided again on the account and the
    /// target as locked in the write.
    pub async fn change_grant(
        &self,
        request: &ChecksRequest,
        proof: &Authorized<CheckNamespaceGrantCtx>,
        grant: bool,
    ) -> Result<GrantOutcome, ForgeError> {
        require(proof.action(), Action::CheckNamespaceGrantManage)?;
        let admin = caller(proof.principal(), Action::CheckNamespaceGrantManage)?;
        let (target, project, namespace) = *proof.id();
        let ctx = change_context(request, proof.principal(), admin);
        let (outcome, ()) = checks::change_check_namespace_grant_decided(
            &self.db,
            &ctx,
            (target, project, namespace, grant),
            admin,
            |facts| {
                let Some(t) = facts.target else {
                    return Err(ForgeError::new(ErrorCode::NotFound)
                        .with_context("resource", "service_account")
                        .with_context("reason", "absent"));
                };
                let view = ServiceAccountView {
                    kind: t.kind,
                    active: t.active,
                };
                let fresh = CheckNamespaceGrantCtx::new(target, view, project, namespace);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        Ok(outcome)
    }

    /// The namespaces `user` holds grants of (the service account read).
    pub async fn grants_of(&self, user: UserId) -> Result<Vec<Grant>, ForgeError> {
        checks::grants_of(&self.db, user).await
    }

    /// The required names among `names` nothing may report on `project`
    /// (the merge rule's `unreportable_checks`): a `forge/` name forge's
    /// own worker does not report (M3 X2: it reports `forge/spec-drift`,
    /// [`forge_core::checks::SYSTEM_REPORTED_CHECKS`]; M4 C5 also
    /// `ci/pipeline`) and every other `ci/` /
    /// `kanflow/` name whose namespace no active service account holds a
    /// grant of on that project.
    pub async fn unreportable(
        &self,
        project: ProjectId,
        names: &[CheckName],
    ) -> Result<Vec<CheckName>, ForgeError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let reported = checks::reported_namespaces(&self.db, project).await?;
        Ok(names
            .iter()
            .filter(|n| !forge_core::checks::system_reports(n) && !reported.contains(&n.namespace()))
            .cloned() // clone: the answer owns its names
            .collect())
    }
}

/// What the doctor's `checks.reporters` reads: how many active service
/// accounts hold each namespace.
pub async fn reporters(
    db: &Db,
) -> Result<std::collections::BTreeMap<CheckNamespace, i64>, ForgeError> {
    checks::reporters(db).await
}
