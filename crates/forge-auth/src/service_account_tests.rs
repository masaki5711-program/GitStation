//! M3 S0 (plan 11 §5 S0, ADR-0019 Addendum S0): the decisions about
//! service accounts, with the log reasons the matrix rows do not show.

use forge_core::{ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use crate::{
    authorize, prove, AccountKind, Action, Credential, LoadedProject, Principal, ProjectCtx,
    ProjectView, Resource, Role, Scope, ServiceAccountTokenRequestCtx, ServiceAccountView,
    Visibility,
};

const ME: UserId = UserId::new(1);
const SA: UserId = UserId::new(7);

fn reason(err: &ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn admin_session() -> Principal {
    Principal::authenticated(ME, true, true, Credential::Session, AccountKind::Human)
}

fn admin_token(scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        ME,
        true,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::Human,
    )
}

fn service_account_token(scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        SA,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::ServiceAccount,
    )
}

fn view(kind: AccountKind, active: bool) -> ServiceAccountView {
    ServiceAccountView { kind, active }
}

fn issue(kind: AccountKind, active: bool, scopes: &[Scope]) -> Resource {
    Resource::ServiceAccountTokenRequest {
        account: view(kind, active),
        scopes: scopes.to_vec(),
    }
}

#[test]
fn service_account_token_issue_is_admin_session_only() {
    let request = issue(AccountKind::ServiceAccount, true, &[Scope::ReadApi]);
    assert!(authorize(&admin_session(), Action::ServiceAccountTokenIssue, &request).is_ok());
    // A person without administrator rights, in a session.
    let person = Principal::authenticated(ME, false, true, Credential::Session, AccountKind::Human);
    let err = authorize(&person, Action::ServiceAccountTokenIssue, &request).expect_err("no admin");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    // The proof carries the account and exactly the scopes decided on.
    let ctx = ServiceAccountTokenRequestCtx::new(
        SA,
        view(AccountKind::ServiceAccount, true),
        vec![Scope::ReadApi],
    );
    let proof = prove(&admin_session(), Action::ServiceAccountTokenIssue, ctx).expect("allowed");
    assert_eq!(*proof.id(), SA);
    assert_eq!(proof.ctx().scopes(), &[Scope::ReadApi]);
    // Creating one is a session's too.
    assert!(authorize(
        &admin_session(),
        Action::ServiceAccountCreate,
        &Resource::Instance
    )
    .is_ok());
    let err = authorize(
        &admin_token(&[Scope::Api, Scope::Admin]),
        Action::ServiceAccountCreate,
        &Resource::Instance,
    )
    .expect_err("a token");
    assert_eq!(reason(&err), "session_required");
}

#[test]
fn token_cannot_issue_service_account_token() {
    let request = issue(AccountKind::ServiceAccount, true, &[Scope::ReadApi]);
    for principal in [
        admin_token(&[Scope::Api, Scope::Admin]),
        admin_token(&[Scope::Api, Scope::Admin]).as_agent(true),
        service_account_token(&[Scope::Api]),
    ] {
        let err = authorize(&principal, Action::ServiceAccountTokenIssue, &request)
            .expect_err("a token never issues a token");
        assert_eq!(err.code(), ErrorCode::InsufficientScope);
        assert_eq!(reason(&err), "session_required");
    }
}

#[test]
fn issue_refused_for_human_or_disabled_target() {
    let decide = |resource: &Resource| {
        authorize(&admin_session(), Action::ServiceAccountTokenIssue, resource)
            .expect_err("refused")
    };
    let human = decide(&issue(AccountKind::Human, true, &[Scope::ReadApi]));
    assert_eq!(human.code(), ErrorCode::NotFound);
    assert_eq!(reason(&human), "not_service_account");
    let bot = decide(&issue(AccountKind::Bot, true, &[Scope::ReadApi]));
    assert_eq!(reason(&bot), "not_service_account");
    let disabled = decide(&issue(
        AccountKind::ServiceAccount,
        false,
        &[Scope::ReadApi],
    ));
    assert_eq!(disabled.code(), ErrorCode::Forbidden);
    assert_eq!(reason(&disabled), "target_disabled");
    let admin = decide(&issue(
        AccountKind::ServiceAccount,
        true,
        &[Scope::ReadApi, Scope::Admin],
    ));
    assert_eq!(reason(&admin), "admin_scope_for_service_account");
    // The other routes find no service account under a person's id either.
    for action in [
        Action::ServiceAccountRead,
        Action::ServiceAccountUpdate,
        Action::ServiceAccountTokenList,
        Action::ServiceAccountTokenRevoke,
    ] {
        let err = authorize(
            &admin_session(),
            action,
            &Resource::ServiceAccount(view(AccountKind::Human, true)),
        )
        .expect_err("a person");
        assert_eq!(reason(&err), "not_service_account", "{action:?}");
    }
}

#[test]
fn service_account_sees_only_member_projects() {
    let reader = service_account_token(&[Scope::ReadApi]);
    let internal = |role| {
        Resource::Project(ProjectView {
            visibility: Visibility::Internal,
            role,
            pending_deletion: false,
        })
    };
    let err = authorize(&reader, Action::ProjectRead, &internal(None)).expect_err("not a member");
    assert_eq!(
        err.code(),
        ErrorCode::NotFound,
        "the same 404 as a private project"
    );
    assert_eq!(reason(&err), "service_account_not_member");
    assert!(authorize(
        &reader,
        Action::ProjectRead,
        &internal(Some(Role::Reporter))
    )
    .is_ok());
    // A Guest member reads by its own role, not as an internal Reporter.
    let err =
        authorize(&reader, Action::ProjectRead, &internal(Some(Role::Guest))).expect_err("guest");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    // The list decides row by row with ProjectRead (through the proof).
    let project = |visibility, role| LoadedProject {
        id: ProjectId::new(3),
        visibility,
        role,
        pending_deletion: false,
    };
    assert!(prove(
        &reader,
        Action::ProjectRead,
        ProjectCtx::new(project(Visibility::Internal, None))
    )
    .is_err());
    assert!(prove(
        &reader,
        Action::ProjectRead,
        ProjectCtx::new(project(Visibility::Private, Some(Role::Developer)))
    )
    .is_ok());
    // A person still reads every internal project.
    let person = Principal::authenticated(
        ME,
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
        AccountKind::Human,
    );
    assert!(authorize(&person, Action::ProjectRead, &internal(None)).is_ok());
}

#[test]
fn service_account_cannot_create_project_in_own_namespace() {
    let creator = service_account_token(&[Scope::Api]);
    let err = authorize(
        &creator,
        Action::ProjectCreate,
        &Resource::Namespace {
            owner: SA,
            owner_kind: AccountKind::ServiceAccount,
        },
    )
    .expect_err("own namespace");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(reason(&err), "non_person_namespace");
    assert!(authorize(
        &creator,
        Action::ProjectCreate,
        &Resource::Group {
            role: Some(Role::Maintainer)
        }
    )
    .is_ok());
}

/// S0 review: the rule is the namespace owner's kind, not the caller's —
/// an administrator is refused too, for creating and for moving into it.
#[test]
fn nobody_creates_or_moves_a_project_into_a_service_accounts_namespace() {
    let theirs = Resource::Namespace {
        owner: SA,
        owner_kind: AccountKind::ServiceAccount,
    };
    let err = authorize(&admin_session(), Action::ProjectCreate, &theirs).expect_err("admin");
    assert_eq!(reason(&err), "non_person_namespace");
    let into = Resource::Transfer {
        source: ProjectView {
            visibility: Visibility::Private,
            role: Some(Role::Owner),
            pending_deletion: false,
        },
        target: crate::TransferTarget::PersonalNamespace {
            owner: SA,
            owner_kind: AccountKind::ServiceAccount,
        },
    };
    let err = authorize(&admin_session(), Action::ProjectTransfer, &into).expect_err("admin");
    assert_eq!(reason(&err), "non_person_namespace");
    // A person's namespace is unchanged: an administrator may.
    let person = Resource::Namespace {
        owner: UserId::new(9),
        owner_kind: AccountKind::Human,
    };
    assert!(authorize(&admin_session(), Action::ProjectCreate, &person).is_ok());
}
