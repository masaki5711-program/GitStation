//! Shared fixtures of the forge-review tests (synthetic users, a group and
//! a project on a fresh migrated database, ADR-0011; branches from a
//! scripted probe instead of a repository).
#![allow(dead_code, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use forge_auth::{
    prove, Action, Authorized, Credential, LoadedMergeRequest, LoadedProject, MergeRequestCtx,
    MergeRequestEdit, Principal,
};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    MergeRequestState, ProjectAccessLoader, ProjectId, ProjectRepository, RefName, RepoId,
    RepositoryState, RequestId, Role, UserId,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, CreatedGroup, NewGroup};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{BranchProbe, Review, ReviewRequest};

/// The branches that exist (full names), changeable during a test.
#[derive(Default)]
pub struct Branches(pub Mutex<BTreeSet<String>>);

impl Branches {
    pub fn with(names: &[&str]) -> Arc<Self> {
        Arc::new(Self(Mutex::new(
            names.iter().map(|n| format!("refs/heads/{n}")).collect(),
        )))
    }

    pub fn delete(&self, name: &str) {
        self.0
            .lock()
            .expect("lock")
            .remove(&format!("refs/heads/{name}"));
    }
}

impl BranchProbe for Branches {
    fn branch_exists<'a>(
        &'a self,
        _repo: RepoId,
        branch: &'a RefName,
        _caller: &'a str,
    ) -> LookupFuture<'a, ()> {
        let found = self.0.lock().expect("lock").contains(branch.as_str());
        Box::pin(async move {
            if found {
                Ok(())
            } else {
                Err(ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                    .with_context("reason", "ref_not_found"))
            }
        })
    }
}

pub fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

pub fn request(id: &str) -> ReviewRequest {
    ReviewRequest {
        request_id: RequestId::parse(id).expect("request id"),
        credential_id: None,
    }
}

pub fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session)
}

pub fn branch(name: &str) -> RefName {
    RefName::parse(&format!("refs/heads/{name}")).expect("branch")
}

pub fn ready(repo: RepoId) -> ProjectRepository {
    ProjectRepository {
        id: repo,
        state: RepositoryState::Ready,
    }
}

pub async fn user(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &system(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
    .user_id
}

/// `acme/web` (private), owned by `owner`, with `members`.
pub async fn project(t: &FreshDb, owner: UserId, members: &[(UserId, Role)]) -> CreatedProject {
    let group: CreatedGroup = groups::create(
        &t.db,
        &system("req-group"),
        &NewGroup {
            segment: "acme".to_owned(),
            parent: None,
            display_name: "Acme".to_owned(),
            description: String::new(),
            owner,
        },
    )
    .await
    .expect("group");
    let web = projects::create(
        &t.db,
        &system("req-project"),
        &NewProject::new("web", group.namespace_id, "Web", owner),
    )
    .await
    .expect("project");
    for (who, role) in members {
        projects::add_member(&t.db, &system("req-member"), web.project_id, *who, *role)
            .await
            .expect("member");
    }
    web
}

/// The project as a loader reads it for `who` now.
pub async fn loaded(t: &FreshDb, who: &Principal, id: ProjectId) -> LoadedProject {
    let record =
        t.db.load_project(id, who.user())
            .await
            .expect("load")
            .expect("project exists");
    LoadedProject {
        id: record.id,
        visibility: record.access.visibility,
        role: record.access.role,
        pending_deletion: record.access.pending_deletion,
    }
}

/// The create route's proof: the caller's new merge request.
pub async fn open_proof(
    t: &FreshDb,
    who: &Principal,
    project: ProjectId,
    (source, target): (&str, &str),
    draft: bool,
) -> Result<Authorized<MergeRequestCtx>, ForgeError> {
    let project = loaded(t, who, project).await;
    prove(
        who,
        Action::MergeRequestCreate,
        MergeRequestCtx::new(
            project,
            LoadedMergeRequest {
                iid: None,
                author: who.user().expect("signed in"),
                state: MergeRequestState::Open,
                draft,
                source_branch: branch(source),
                target_branch: branch(target),
            },
        ),
    )
}

/// The proof of a route on stored merge request `iid` (`edit` for an
/// update).
pub async fn stored_proof(
    t: &FreshDb,
    review: &Review,
    who: &Principal,
    action: Action,
    project: ProjectId,
    iid: i64,
    edit: MergeRequestEdit,
) -> Result<Authorized<MergeRequestCtx>, ForgeError> {
    let loaded_project = loaded(t, who, project).await;
    let mr = review
        .merge_request(project, iid)
        .await
        .expect("load")
        .expect("merge request exists");
    prove(
        who,
        action,
        MergeRequestCtx::edited(
            loaded_project,
            forge_review::loaded_merge_request(&mr),
            edit,
        ),
    )
}

/// The rows of `sql` (a count named n) as the superuser.
pub async fn count(t: &FreshDb, sql: &str) -> i64 {
    let rows = t.json_rows(sql).await;
    rows.first()
        .and_then(|r| r.get("n"))
        .and_then(serde_json::Value::as_i64)
        .expect("a count named n")
}
