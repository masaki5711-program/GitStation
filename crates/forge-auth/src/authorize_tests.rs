use std::collections::BTreeMap;

use forge_core::UserId;
use forge_errors::ErrorCode;

use crate::matrix::{summary, Actor, Expect, Fixture, ROWS};
use crate::{
    authorize, Action, Principal, ProjectView, RefUpdate, Resource, Role, Scope, Visibility,
};

fn decide(action: Action, actor: Actor, fixture: Fixture) -> Expect {
    match authorize(&actor.principal(), action, &fixture.resource(actor)) {
        Ok(()) => Expect::Allow,
        Err(e) => Expect::Deny(e.code()),
    }
}

/// Every row of the matrix holds — allowed and denied alike.
#[test]
fn denied_matrix_rows_all_deny() {
    let mut failures = Vec::new();
    for r in ROWS {
        let got = decide(r.action, r.actor, r.fixture);
        if got != r.expect {
            failures.push(format!(
                "{:?} {:?} {:?}: expected {:?}, got {:?}",
                r.action, r.actor, r.fixture, r.expect, got
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "matrix rows violated:\n{}",
        failures.join("\n")
    );
    assert!(ROWS.iter().any(|r| matches!(r.expect, Expect::Deny(_))));
}

#[test]
fn every_action_has_matrix_rows() {
    let mut missing = Vec::new();
    for s in summary() {
        let ok = if s.public {
            s.allow > 0
        } else {
            s.allow > 0 && s.deny > 0
        };
        if !ok {
            missing.push(format!("{} (allow {}, deny {})", s.action, s.allow, s.deny));
        }
    }
    assert!(
        missing.is_empty(),
        "actions without allow+deny rows: {missing:?}"
    );
    assert_eq!(summary().len(), Action::ALL.len());
}

#[test]
fn action_names_are_unique() {
    let mut seen = BTreeMap::new();
    for a in Action::ALL {
        assert!(
            seen.insert(a.name(), a).is_none(),
            "duplicate name {}",
            a.name()
        );
    }
}

#[test]
fn anonymous_is_unauthenticated() {
    for action in Action::ALL.into_iter().filter(|a| !a.is_public()) {
        let err = authorize(&Principal::anonymous(), action, &Resource::Instance)
            .expect_err("no anonymous access");
        assert_eq!(err.code(), ErrorCode::Unauthenticated, "{action:?}");
    }
    assert!(authorize(&Principal::anonymous(), Action::Health, &Resource::Instance).is_ok());
}

#[test]
fn disabled_account_is_denied() {
    let mut p = Principal::user(UserId::new(1));
    p.active = false;
    p.is_admin = true; // even an administrator
    let err = authorize(&p, Action::UserCreate, &Resource::Instance).expect_err("disabled");
    assert_eq!(err.code(), ErrorCode::AccountDisabled);
}

#[test]
fn invisible_private_project_is_not_found() {
    let project = Resource::Project(ProjectView {
        visibility: Visibility::Private,
        role: None,
        pending_deletion: false,
    });
    // Every project-scoped action gives the same 404 — no existence oracle.
    for action in [
        Action::RepoRead,
        Action::ProjectRead,
        Action::ProjectUpdate,
        Action::ProjectDelete,
        Action::ProtectedRefManage,
    ] {
        let err =
            authorize(&Principal::user(UserId::new(1)), action, &project).expect_err("not visible");
        assert_eq!(err.code(), ErrorCode::NotFound, "{action:?}");
    }
}

#[test]
fn pending_deletion_blocks_everything_but_restore() {
    let deleted = Resource::Project(ProjectView {
        visibility: Visibility::Private,
        role: Some(Role::Owner),
        pending_deletion: true,
    });
    let owner = Principal::user(UserId::new(1));
    for action in [
        Action::RepoRead,
        Action::ProjectRead,
        Action::ProjectUpdate,
        Action::ProjectDelete,
    ] {
        let err = authorize(&owner, action, &deleted).expect_err("pending deletion");
        assert_eq!(err.code(), ErrorCode::PendingDeletion, "{action:?}");
    }
    assert!(authorize(&owner, Action::ProjectRestore, &deleted).is_ok());
}

#[test]
fn token_scope_narrows_role() {
    // An Owner with a read_api token may read but not change anything.
    let mut p = Principal::user(UserId::new(1));
    p.token_scopes = Some(vec![Scope::ReadApi]);
    let project = Resource::Project(ProjectView {
        visibility: Visibility::Private,
        role: Some(Role::Owner),
        pending_deletion: false,
    });
    assert!(authorize(&p, Action::ProjectRead, &project).is_ok());
    let err = authorize(&p, Action::ProjectDelete, &project).expect_err("needs api scope");
    assert_eq!(err.code(), ErrorCode::InsufficientScope);
    assert!(err.to_string().contains("required_scope=api"), "{err}");
}

#[test]
fn scope_cannot_exceed_role() {
    // A Reporter with an `api` token still cannot do Maintainer things.
    let mut p = Principal::user(UserId::new(1));
    p.token_scopes = Some(vec![Scope::Api, Scope::WriteRepository]);
    let project = Resource::Project(ProjectView {
        visibility: Visibility::Private,
        role: Some(Role::Reporter),
        pending_deletion: false,
    });
    let err = authorize(&p, Action::ProjectUpdate, &project).expect_err("role too low");
    assert_eq!(err.code(), ErrorCode::Forbidden);
}

#[test]
fn force_push_to_protected_is_denied_even_for_admin() {
    assert_eq!(
        decide(
            Action::RepoPush,
            Actor::Admin,
            Fixture::Ref {
                update: RefUpdate::Force,
                protected: Some(Role::Owner)
            }
        ),
        Expect::Deny(ErrorCode::ProtectedRefForcePush)
    );
}

#[test]
fn push_below_protection_min_role_is_denied() {
    assert_eq!(
        decide(
            Action::RepoPush,
            Actor::Developer,
            Fixture::Ref {
                update: RefUpdate::FastForward,
                protected: Some(Role::Maintainer)
            }
        ),
        Expect::Deny(ErrorCode::ProtectedRefPushNotAllowed)
    );
}

#[test]
fn delete_protected_ref_is_denied() {
    assert_eq!(
        decide(
            Action::RepoPush,
            Actor::Owner,
            Fixture::Ref {
                update: RefUpdate::Delete,
                protected: Some(Role::Developer)
            }
        ),
        Expect::Deny(ErrorCode::ProtectedRefDeletion)
    );
}

#[test]
fn mismatched_resource_is_an_internal_error_not_an_allow() {
    let admin = Actor::Admin.principal();
    let err = authorize(&admin, Action::RepoPush, &Resource::Instance).expect_err("wrong shape");
    assert_eq!(err.code(), ErrorCode::Internal);
}

#[test]
fn denial_never_mentions_resource_names_or_paths() {
    // Context carries action / role names only (safe for logs, never shown to users).
    let err = authorize(
        &Principal::user(UserId::new(1)),
        Action::ProjectUpdate,
        &Resource::Project(ProjectView {
            visibility: Visibility::Private,
            role: Some(Role::Developer),
            pending_deletion: false,
        }),
    )
    .expect_err("forbidden");
    let keys: Vec<&str> = err.context().iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, ["action", "required_role", "actual_role"]);
}

#[test]
fn summary_json_lists_every_action() {
    let json = crate::matrix::summary_json();
    for a in Action::ALL {
        assert!(
            json.contains(&format!("\"action\": \"{}\"", a.name())),
            "{a:?}"
        );
    }
    assert!(json.contains("\"git_operation\": true"));
}
