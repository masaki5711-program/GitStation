#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The check run commands against a migrated database (M3 C1, ADR-0024
//! §10): each write is decided by the route's proof and again under the
//! locks; the commit is checked for reachability first and nothing is
//! written when it is not reachable or the check cannot decide.

use std::sync::{Arc, Mutex};

use forge_auth::{
    prove, AccountKind, Action, Authorized, CheckNamespaceGrantCtx, CheckRunCtx, Credential,
    LoadedProject, Principal, ProjectCtx, Scope, ServiceAccountView,
};
use forge_checks::{parse_name, Checks, ChecksRequest, CommitProbe, ReportInput};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    CheckNamespace, ObjectId, ProjectAccessLoader, ProjectId, ProjectRepository, RepoId,
    RepositoryState, RequestId, Role, UserId,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::GrantOutcome;
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::service_accounts::{self, NewServiceAccount};
use forge_db::test_support::{assert_reason, FreshDb};
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};

const HEAD: &str = "1111111111111111111111111111111111111111";

/// The commits a branch or tag reaches (scripted), or the error the
/// reachability check answers with.
#[derive(Default)]
struct Commits {
    reachable: Mutex<Vec<String>>,
    fail_with: Mutex<Option<ErrorCode>>,
}

impl Commits {
    fn with(shas: &[&str]) -> Arc<Self> {
        let c = Self::default();
        *c.reachable.lock().expect("lock") = shas.iter().map(|s| (*s).to_owned()).collect();
        Arc::new(c)
    }
}

impl CommitProbe for Commits {
    fn reachable_commit<'a>(
        &'a self,
        _repo: RepoId,
        sha: &'a ObjectId,
        _caller: &'a str,
    ) -> LookupFuture<'a, ObjectId> {
        let fail = *self.fail_with.lock().expect("lock");
        let found = self
            .reachable
            .lock()
            .expect("lock")
            .iter()
            .any(|s| s == sha.as_str());
        Box::pin(async move {
            if let Some(code) = fail {
                return Err(ForgeError::new(code).with_context("check", "reachability"));
            }
            if found {
                Ok(sha.clone())
            } else {
                Err(ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                    .with_context("reason", "ref_not_found"))
            }
        })
    }
}

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn request(id: &str) -> ChecksRequest {
    ChecksRequest {
        request_id: RequestId::parse(id).expect("request id"),
        credential_id: None,
    }
}

fn token(user: UserId, kind: AccountKind, scopes: &[Scope]) -> Principal {
    Principal::authenticated(user, false, true, Credential::Token(scopes.to_vec()), kind)
}

fn admin_session(user: UserId) -> Principal {
    Principal::authenticated(user, true, true, Credential::Session, AccountKind::Human)
}

fn sha(raw: &str) -> ObjectId {
    ObjectId::parse(raw).expect("sha")
}

fn report(status: &str, conclusion: Option<&str>) -> ReportInput {
    ReportInput {
        status: status.to_owned(),
        conclusion: conclusion.map(str::to_owned),
        summary: Some("synthetic".to_owned()),
        details_url: None,
        annotations: Vec::new(),
    }
}

struct World {
    t: FreshDb,
    admin: UserId,
    ci: UserId,
    web: CreatedProject,
    /// Another project the service account is a Developer member of.
    other: CreatedProject,
    checks: Checks,
    commits: Arc<Commits>,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let mut ada = NewUser::human("ada", "Ada", "ada@example.test");
    ada.is_admin = true;
    let admin = users::create(&t.db, &system("req-user-ada"), &ada)
        .await
        .expect("ada");
    let ci = service_accounts::create(
        &t.db,
        &system("req-sa"),
        &NewServiceAccount {
            username: "ci-runner".to_owned(),
            display_name: "CI".to_owned(),
            owner: admin.user_id,
            purpose: "synthetic CI reporter".to_owned(),
        },
        admin.user_id,
    )
    .await
    .expect("ci");
    let web = projects::create(
        &t.db,
        &system("req-project"),
        &NewProject::new("web", admin.namespace_id, "Web", admin.user_id),
    )
    .await
    .expect("web");
    projects::add_member(
        &t.db,
        &system("req-member"),
        web.project_id,
        ci.user_id,
        Role::Developer,
    )
    .await
    .expect("member");
    let other = projects::create(
        &t.db,
        &system("req-project-other"),
        &NewProject::new("other", admin.namespace_id, "Other", admin.user_id),
    )
    .await
    .expect("other");
    projects::add_member(
        &t.db,
        &system("req-member-other"),
        other.project_id,
        ci.user_id,
        Role::Developer,
    )
    .await
    .expect("member");
    let commits = Commits::with(&[HEAD]);
    let checks = Checks::new(t.db.clone(), commits.clone());
    World {
        t,
        admin: admin.user_id,
        ci: ci.user_id,
        web,
        other,
        checks,
        commits,
    }
}

impl World {
    fn repo(&self) -> ProjectRepository {
        ProjectRepository {
            id: self.web.repository_id,
            state: RepositoryState::Ready,
        }
    }

    fn reporter(&self) -> Principal {
        token(self.ci, AccountKind::ServiceAccount, &[Scope::Api])
    }

    async fn loaded(&self, who: &Principal) -> LoadedProject {
        self.loaded_on(self.web.project_id, who).await
    }

    async fn loaded_on(&self, project: ProjectId, who: &Principal) -> LoadedProject {
        let record = self
            .t
            .db
            .load_project(project, who.user())
            .await
            .expect("load")
            .expect("project");
        LoadedProject {
            id: record.id,
            visibility: record.access.visibility,
            role: record.access.role,
            pending_deletion: record.access.pending_deletion,
        }
    }

    async fn grant(&self, on: bool) -> GrantOutcome {
        self.grant_ns(CheckNamespace::Ci, on).await
    }

    async fn grant_ns(&self, namespace: CheckNamespace, on: bool) -> GrantOutcome {
        let admin = admin_session(self.admin);
        let view = ServiceAccountView {
            kind: AccountKind::ServiceAccount,
            active: true,
        };
        let proof = prove(
            &admin,
            Action::CheckNamespaceGrantManage,
            CheckNamespaceGrantCtx::new(self.ci, view, self.web.project_id, namespace),
        )
        .expect("an administrator manages grants");
        self.checks
            .change_grant(&request("req-grant"), &proof, on)
            .await
            .expect("grant")
    }

    /// The report route's proof for `who` on `head` / `name`, as its
    /// loader decides it now.
    async fn proof(
        &self,
        who: &Principal,
        head: &str,
        name: &str,
    ) -> Result<Authorized<CheckRunCtx>, ForgeError> {
        self.proof_on(self.web.project_id, who, head, name).await
    }

    async fn proof_on(
        &self,
        project: ProjectId,
        who: &Principal,
        head: &str,
        name: &str,
    ) -> Result<Authorized<CheckRunCtx>, ForgeError> {
        let name = parse_name(name).expect("name");
        let user = who.user().expect("signed in");
        let granted = self
            .checks
            .granted(user, project, name.namespace())
            .await
            .expect("grant read");
        prove(
            who,
            Action::CheckWrite,
            CheckRunCtx::new(self.loaded_on(project, who).await, sha(head), name, granted),
        )
    }

    async fn runs(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM forge.check_runs WHERE project_id = $1")
            .bind(self.web.project_id.get())
            .fetch_one(&mut self.t.superuser().await)
            .await
            .expect("count")
    }

    fn project(&self) -> ProjectId {
        self.web.project_id
    }
}

#[tokio::test]
async fn a_granted_service_account_reports_and_reads_its_run() {
    let w = world().await;
    assert_eq!(w.grant(true).await, GrantOutcome::Changed);
    let who = w.reporter();
    let proof = w.proof(&who, HEAD, "ci/gates").await.expect("allowed");
    let reported = w
        .checks
        .report(
            &request("req-1"),
            &proof,
            w.repo(),
            &report("completed", Some("success")),
        )
        .await
        .expect("report");
    assert!(reported.created);
    assert!(reported.completed_event);
    assert_eq!(reported.run.reported_by, Some(w.ci));
    let read = prove(
        &token(w.ci, AccountKind::ServiceAccount, &[Scope::ReadApi]),
        Action::CheckRead,
        ProjectCtx::new(w.loaded(&who).await),
    )
    .expect("a member reads");
    let runs = w
        .checks
        .list(&read, w.repo(), &sha(HEAD))
        .await
        .expect("list");
    assert_eq!(runs, vec![reported.run]);
    // The audit row names the service account as the actor.
    let actor: (String, Option<i64>) = sqlx::query_as(
        "SELECT actor_kind, actor_user_id FROM forge.audit_log WHERE action = 'check.completed'",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("audit");
    assert_eq!(actor, ("service_account".to_owned(), Some(w.ci.get())));
    w.t.drop_now().await;
}

/// The grant is read again under the lock: revoked after the proof, the
/// write is denied and nothing is written.
#[tokio::test]
async fn a_grant_revoked_after_the_proof_denies_the_write() {
    let w = world().await;
    assert_eq!(w.grant(true).await, GrantOutcome::Changed);
    let who = w.reporter();
    let stale = w.proof(&who, HEAD, "ci/gates").await.expect("allowed");
    assert_eq!(w.grant(false).await, GrantOutcome::Changed);
    let err = w
        .checks
        .report(&request("req-1"), &stale, w.repo(), &report("queued", None))
        .await
        .expect_err("revoked");
    assert_reason(&err, ErrorCode::Forbidden, "namespace_not_granted");
    assert_eq!(w.runs().await, 0);
    w.t.drop_now().await;
}

/// The membership is read again under the lock too.
#[tokio::test]
async fn a_membership_removed_after_the_proof_denies_the_write() {
    let w = world().await;
    assert_eq!(w.grant(true).await, GrantOutcome::Changed);
    let who = w.reporter();
    let stale = w.proof(&who, HEAD, "ci/gates").await.expect("allowed");
    projects::remove_member(
        &w.t.db,
        &system("req-remove"),
        w.project(),
        w.ci,
        Role::Developer,
    )
    .await
    .expect("remove");
    let err = w
        .checks
        .report(&request("req-1"), &stale, w.repo(), &report("queued", None))
        .await
        .expect_err("no longer a member");
    assert_eq!(err.code(), ErrorCode::NotFound, "{err}");
    assert_eq!(w.runs().await, 0);
    w.t.drop_now().await;
}

/// An unreachable commit is answered like a missing one; a reachability
/// check that cannot decide (its time limit) never records the run.
#[tokio::test]
async fn an_unreachable_or_undecided_commit_records_nothing() {
    let w = world().await;
    assert_eq!(w.grant(true).await, GrantOutcome::Changed);
    let who = w.reporter();
    let other = "2222222222222222222222222222222222222222";
    let proof = w.proof(&who, other, "ci/gates").await.expect("allowed");
    let err = w
        .checks
        .report(&request("req-1"), &proof, w.repo(), &report("queued", None))
        .await
        .expect_err("unreachable");
    assert_reason(&err, ErrorCode::RepositoryPathNotResolved, "ref_not_found");
    *w.commits.fail_with.lock().expect("lock") = Some(ErrorCode::GitTimeout);
    let proof = w.proof(&who, HEAD, "ci/gates").await.expect("allowed");
    let err = w
        .checks
        .report(&request("req-2"), &proof, w.repo(), &report("queued", None))
        .await
        .expect_err("timeout");
    assert_eq!(err.code(), ErrorCode::GitTimeout);
    assert_eq!(w.runs().await, 0);
    w.t.drop_now().await;
}

/// The body's rules are checked after the decision: a caller who may not
/// report never reaches them (the proof does not exist), an allowed one
/// gets `FG-REVIEW-0020` and nothing is written.
#[tokio::test]
async fn an_invalid_body_is_refused_after_the_decision() {
    let w = world().await;
    let who = w.reporter();
    // Without a grant there is no proof at all.
    let err = w.proof(&who, HEAD, "ci/gates").await.expect_err("no grant");
    assert_reason(&err, ErrorCode::Forbidden, "namespace_not_granted");
    assert_eq!(w.grant(true).await, GrantOutcome::Changed);
    let proof = w.proof(&who, HEAD, "ci/gates").await.expect("allowed");
    let err = w
        .checks
        .report(
            &request("req-1"),
            &proof,
            w.repo(),
            &report("completed", None),
        )
        .await
        .expect_err("no conclusion");
    assert_reason(
        &err,
        ErrorCode::CheckPayloadInvalid,
        "completed_without_conclusion",
    );
    assert_eq!(w.runs().await, 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn unreportable_names_are_the_ones_without_a_granted_reporter() {
    let w = world().await;
    // M3 X2: `forge/spec-drift` is reported by forge's own worker, so it is
    // never unreportable; another `forge/` name has no reporter yet.
    let drift = parse_name("forge/spec-drift").expect("name");
    let names: Vec<_> = ["ci/gates", "kanflow/work-item", "forge/deploy-gate"]
        .iter()
        .map(|n| parse_name(n).expect("name"))
        .collect();
    let mut with_drift = names.clone();
    with_drift.push(drift);
    assert_eq!(
        w.checks
            .unreportable(w.project(), &with_drift)
            .await
            .expect("all"),
        names
    );
    assert_eq!(w.grant(true).await, GrantOutcome::Changed);
    assert_eq!(
        w.checks
            .unreportable(w.project(), &names)
            .await
            .expect("ci granted"),
        names[1..].to_vec()
    );
    assert!(w
        .checks
        .unreportable(w.project(), &[])
        .await
        .expect("none")
        .is_empty());
    w.t.drop_now().await;
}

/// Review fix 1 (security MEDIUM): a grant is for one project. A service
/// account granted `ci` for `web` that a Maintainer of `other` adds as a
/// Developer cannot report `ci/` there; a `kanflow` grant never covers
/// `ci/`.
#[tokio::test]
async fn a_grant_on_one_project_never_reports_on_another() {
    let w = world().await;
    assert_eq!(w.grant(true).await, GrantOutcome::Changed);
    let who = w.reporter();
    let err = w
        .proof_on(w.other.project_id, &who, HEAD, "ci/gates")
        .await
        .expect_err("granted for web only");
    assert_reason(&err, ErrorCode::Forbidden, "namespace_not_granted");
    assert_eq!(
        w.grant_ns(CheckNamespace::Kanflow, true).await,
        GrantOutcome::Changed
    );
    assert_eq!(w.grant(false).await, GrantOutcome::Changed);
    let err = w
        .proof(&who, HEAD, "ci/gates")
        .await
        .expect_err("kanflow does not cover ci");
    assert_reason(&err, ErrorCode::Forbidden, "namespace_not_granted");
    let proof = w
        .proof(&who, HEAD, "kanflow/work-item")
        .await
        .expect("kanflow is granted");
    assert_eq!(proof.action(), Action::CheckWrite);
    w.t.drop_now().await;
}
