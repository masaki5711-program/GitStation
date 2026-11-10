//! The CI principals (M4 C0, ADR-0029): the job arm of `authorize()`,
//! `authorize_pinned_copy`, `Action::ci_rule` and the matrix's CI rows.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

use std::path::{Path, PathBuf};

use forge_errors::{ErrorCode, ForgeError};

use crate::action::{Action, CiRule};
use crate::matrix::{Actor, Expect, Fixture, ME, ROWS};
use crate::matrix_rows_ci::{job, runner, sha, JOB_PROJECT, JOB_SHA};
use crate::model::{
    AccountKind, CopyMark, Credential, JobTokenState, Principal, ProjectView, Resource, Role,
    Scope, Visibility,
};
use crate::{authorize, authorize_pinned_copy};
use forge_core::ProjectId;

fn ctx<'a>(err: &'a ForgeError, key: &str) -> Option<&'a str> {
    err.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

fn copy(mark: CopyMark) -> Resource {
    Resource::RepoCopy {
        project: ProjectView {
            visibility: Visibility::Private,
            role: Some(Role::Developer),
            pending_deletion: false,
        },
        mark,
    }
}

fn token(kind: AccountKind, agent: bool) -> Principal {
    Principal::authenticated(
        ME,
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
        kind,
    )
    .as_agent(agent)
}

#[test]
fn job_token_clones_own_confidential_repo() {
    // The pinned copy never carries a mark: the job's own project at its
    // own SHA is allowed whatever the tip holds (ADR-0029 §3) …
    let live = job(JobTokenState::Live);
    assert!(authorize_pinned_copy(&live, JOB_PROJECT, &sha(JOB_SHA)).is_ok());
    // … although the job is (and stays) an agent audience: the job arm
    // decides first, so FG-AUTH-0022 is never reached (§4).
    assert!(live.is_agent_audience());
    assert!(!live.admin_rights());
    assert!(live.user().is_none());
    assert!(live.scope_names().is_empty());
    // The measured copy is a person's or an agent's route, not a job's:
    // the invisible project's 404, the cause in the log only.
    for mark in [CopyMark::Open, CopyMark::Confidential] {
        let err = authorize(&live, Action::RepoRead, &copy(mark)).expect_err("measured copy");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert_eq!(ctx(&err, "reason"), Some("job_requires_pinned_copy"));
        assert_eq!(ctx(&err, "credential"), Some("job"));
        assert_eq!(ctx(&err, "job_id"), Some("41"));
    }
}

#[test]
fn job_token_other_project_or_sha_is_not_found() {
    let live = job(JobTokenState::Live);
    let other_project = authorize_pinned_copy(&live, ProjectId::new(8), &sha(JOB_SHA))
        .expect_err("another project");
    assert_eq!(other_project.code(), ErrorCode::NotFound);
    assert_eq!(ctx(&other_project, "reason"), Some("job_other_project"));
    assert_eq!(ctx(&other_project, "project_id"), Some("8"));
    let other_sha = authorize_pinned_copy(
        &live,
        JOB_PROJECT,
        &sha("2222222222222222222222222222222222222222"),
    )
    .expect_err("another sha");
    assert_eq!(other_sha.code(), ErrorCode::NotFound);
    assert_eq!(ctx(&other_sha, "reason"), Some("job_sha_mismatch"));
    assert_eq!(ctx(&other_sha, "action"), Some("RepoRead"));
    assert_eq!(ctx(&other_sha, "ci_rule"), Some("job"));
}

#[test]
fn expired_job_is_unauthenticated() {
    for (state, reason) in [
        (JobTokenState::Expired, "expired"),
        (JobTokenState::Revoked, "revoked"),
    ] {
        let err =
            authorize_pinned_copy(&job(state), JOB_PROJECT, &sha(JOB_SHA)).expect_err("not live");
        assert_eq!(err.code(), ErrorCode::Unauthenticated, "{state:?}");
        assert_eq!(ctx(&err, "reason"), Some(reason));
        assert_eq!(ctx(&err, "credential"), Some("job"));
        // Before the action's rule: a dead token learns nothing else.
        let err = authorize(&job(state), Action::Health, &Resource::Instance).expect_err("dead");
        assert_eq!(err.code(), ErrorCode::Unauthenticated, "{state:?}");
    }
}

#[test]
fn runner_token_cannot_read_repo() {
    let runner = runner();
    let private = Resource::Project(ProjectView {
        visibility: Visibility::Private,
        role: Some(Role::Owner),
        pending_deletion: false,
    });
    for resource in [copy(CopyMark::Open), private] {
        let err = authorize(&runner, Action::RepoRead, &resource).expect_err("runner");
        assert_eq!(err.code(), ErrorCode::Forbidden);
        assert_eq!(ctx(&err, "reason"), Some("ci_not_allowed"));
        assert_eq!(ctx(&err, "credential"), Some("runner"));
        assert_eq!(ctx(&err, "runner_id"), Some("3"));
    }
    let err = authorize_pinned_copy(&runner, JOB_PROJECT, &sha(JOB_SHA)).expect_err("pinned");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(!runner.admin_rights());
}

#[test]
fn service_account_still_withheld() {
    // FG-AUTH-0022 is unchanged for service accounts, bots and agents'
    // tokens (ADR-0029 §6) …
    for p in [
        token(AccountKind::ServiceAccount, false),
        token(AccountKind::Bot, false),
        token(AccountKind::Human, true),
    ] {
        let err =
            authorize(&p, Action::RepoRead, &copy(CopyMark::Confidential)).expect_err("withheld");
        assert_eq!(err.code(), ErrorCode::RepositoryCopyWithheld);
        assert!(authorize(&p, Action::RepoRead, &copy(CopyMark::Open)).is_ok());
        // … and none of them gets a job's pinned copy.
        let err = authorize_pinned_copy(&p, JOB_PROJECT, &sha(JOB_SHA)).expect_err("no job");
        assert_eq!(err.code(), ErrorCode::Forbidden);
        assert_eq!(ctx(&err, "reason"), Some("pinned_copy_without_job"));
    }
    // A person still copies whatever the mark.
    let person = token(AccountKind::Human, false);
    assert!(authorize(&person, Action::RepoRead, &copy(CopyMark::Confidential)).is_ok());
    assert!(!person.is_agent_audience());
}

#[test]
fn ci_rule_is_exhaustive_and_default_deny() {
    // Only the job's copy is open to CI; no runner action before M4 C3.
    let open: Vec<Action> = Action::ALL
        .iter()
        .copied()
        .filter(|a| a.ci_rule() != CiRule::Denied)
        .collect();
    assert_eq!(open, vec![Action::RepoRead]);
    assert_eq!(Action::RepoRead.ci_rule(), CiRule::Job);
    for &a in Action::ALL {
        // Every action is refused to a live job and to a runner on a
        // plain resource (public ones included: the CI arm comes first);
        // the act-as confirmation hides every refusal as its 404 …
        let refusal = if matches!(a, Action::ActAsConfirmView | Action::ActAsAssertionIssue) {
            ErrorCode::NotFound
        } else {
            ErrorCode::Forbidden
        };
        for (p, is_job) in [(job(JobTokenState::Live), true), (runner(), false)] {
            let err = authorize(&p, a, &Resource::Instance).expect_err("closed");
            // A job's own action on anything but its pinned copy is 404.
            let expected = if is_job && a.ci_rule() == CiRule::Job {
                ErrorCode::NotFound
            } else {
                refusal
            };
            assert_eq!(err.code(), expected, "{a:?}");
        }
        // … and has its generated matrix row in both columns.
        if a.ci_rule() != CiRule::Job {
            assert!(
                ROWS.iter().any(|r| r.action == a
                    && r.actor == Actor::Job(JobTokenState::Live)
                    && r.expect == Expect::Deny(refusal)),
                "ci_denied_for_{}",
                a.name()
            );
        }
        assert!(
            ROWS.iter().any(|r| r.action == a
                && r.actor == Actor::Runner
                && r.fixture == Fixture::PrivateProject
                && r.expect == Expect::Deny(refusal)),
            "runner ci_denied_for_{}",
            a.name()
        );
    }
    // The job's one allowed row exists, and it is the only CI allow row.
    let allowed: Vec<_> = ROWS
        .iter()
        .filter(|r| matches!(r.actor, Actor::Job(_) | Actor::Runner) && r.expect == Expect::Allow)
        .collect();
    assert_eq!(allowed.len(), 1, "{allowed:?}");
}

/// Security review of C0: a job asking for anything but its pinned copy
/// learns nothing about the project — a visible (internal) and a hidden
/// (private, or absent) one give the same status and the same user body.
#[test]
fn a_job_sees_the_same_404_for_visible_and_hidden_projects() {
    let live = job(JobTokenState::Live);
    let view = |visibility| ProjectView {
        visibility,
        role: None,
        pending_deletion: false,
    };
    let answers: Vec<ForgeError> = [
        Resource::Project(view(Visibility::Internal)),
        Resource::Project(view(Visibility::Private)),
        Resource::RepoCopy {
            project: view(Visibility::Internal),
            mark: CopyMark::Open,
        },
        Resource::Unresolved(crate::model::UnresolvedReason::Absent),
    ]
    .iter()
    .map(|r| authorize(&live, Action::RepoRead, r).expect_err("not the pinned copy"))
    .collect();
    let other = authorize_pinned_copy(&live, ProjectId::new(8), &sha(JOB_SHA)).expect_err("other");
    for err in answers.iter().chain([&other]) {
        assert_eq!(err.code(), ErrorCode::NotFound, "{err:?}");
        for lang in [forge_errors::Lang::Ja, forge_errors::Lang::En] {
            assert_eq!(err.user_message(lang), other.user_message(lang));
        }
    }
}

/// Authorization review of C0: a CI credential put into
/// `Principal::authenticated` (which always carries a user) is a defect
/// at the decision point, never a pass.
#[test]
fn a_ci_credential_with_a_user_is_an_internal_error() {
    for ci in [job(JobTokenState::Live), runner()] {
        let forged =
            Principal::authenticated(ME, true, true, ci.credential().clone(), AccountKind::Human);
        for (action, resource) in [
            (
                Action::RepoRead,
                crate::matrix_rows_ci::pinned_copy(false, false),
            ),
            (Action::Health, Resource::Instance),
        ] {
            let err = authorize(&forged, action, &resource).expect_err("forged");
            assert_eq!(err.code(), ErrorCode::Internal);
            assert_eq!(ctx(&err, "reason"), Some("ci_credential_with_user"));
        }
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `crate/relative/path` under `crates/` whose code (not its
/// comments) contains one of `needles`.
fn mentions(needles: &[String]) -> Vec<String> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates/") {
        let dir = entry.expect("entry").path();
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        let mut files = Vec::new();
        rust_files(&dir, &mut files);
        for file in files {
            let text = std::fs::read_to_string(&file).expect("read");
            let code: String = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            if needles.iter().any(|n| code.contains(n.as_str())) {
                let rel = file
                    .strip_prefix(&dir)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                found.push(format!("{name}/{rel}"));
            }
        }
    }
    found.sort();
    found
}

/// ADR-0029 §1: a runner or job principal is built by forge-web's
/// authentication only (M4 C3 adds that caller); a worker, a bot or a
/// queued job can never mint one. Rust cannot hide a constructor across
/// crates, so this scan is the guard (as `only_forge_web_builds_act_as_principals`).
#[test]
fn only_forge_web_builds_ci_principals() {
    // Assembled so this file's own code does not match the needles.
    // `authorize()` also refuses a CI credential that carries a user (it
    // came through `Principal::authenticated`, test
    // `a_ci_credential_with_a_user_is_an_internal_error`), and
    // `Resource::PinnedCopy` is unconstructible outside forge-auth.
    let needles = [
        ["Principal::", "runner("].concat(),
        ["Principal::", "job("].concat(),
        ["Credential::", "Runner(RunnerCredential"].concat(),
        ["Credential::", "Job(Box"].concat(),
        ["JobCredential", " {"].concat(),
        ["RunnerCredential", " {"].concat(),
        ["Credential::", "*"].concat(),
    ];
    let allowed = [
        // The constructors' own module and the matrix's fixtures.
        "forge-auth/src/model.rs",
        "forge-auth/src/matrix_rows_ci.rs",
        // forge-review's proof that a CI principal is never audited as a
        // user.
        "forge-review/src/change_context_tests.rs",
        // The production caller (M4 C3): the authentication layer.
        "forge-web/src/auth.rs",
    ];
    let offenders: Vec<_> = mentions(&needles)
        .into_iter()
        .filter(|f| !allowed.contains(&f.as_str()))
        .collect();
    assert!(
        offenders.is_empty(),
        "only forge-web's authentication builds a CI principal: {offenders:?}"
    );
}
