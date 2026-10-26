use std::collections::BTreeSet;

use forge_errors::ErrorCode;

use crate::matrix::{summary, Actor, Expect, Fixture, ME, ROWS};
use crate::{
    authorize, Action, Credential, MembershipChange, MembershipScope, Principal, ProjectView,
    RefKind, RefProtection, RefUpdate, Resource, Role, Scope, Visibility,
};

fn decide(action: Action, actor: Actor, fixture: Fixture) -> Expect {
    match authorize(&actor.principal(), action, &fixture.resource(actor)) {
        Ok(()) => Expect::Allow,
        Err(e) => Expect::Deny(e.code()),
    }
}

fn session() -> Principal {
    Principal::authenticated(ME, false, true, Credential::Session)
}

fn project(role: Option<Role>) -> ProjectView {
    ProjectView {
        visibility: Visibility::Private,
        role,
        pending_deletion: false,
    }
}

/// Every row of the matrix holds — allowed and denied alike.
#[test]
fn denied_matrix_rows_all_deny() {
    let failures: Vec<String> = ROWS
        .iter()
        .filter_map(|r| {
            let got = decide(r.action, r.actor, r.fixture);
            (got != r.expect).then(|| {
                format!(
                    "{:?} {:?} {:?}: expected {:?}, got {:?}",
                    r.action, r.actor, r.fixture, r.expect, got
                )
            })
        })
        .collect();
    assert!(
        failures.is_empty(),
        "matrix rows violated:\n{}",
        failures.join("\n")
    );
}

#[test]
fn every_action_has_matrix_rows() {
    let missing: Vec<String> = summary()
        .into_iter()
        .filter(|s| !(s.allow > 0 && (s.public || s.deny > 0)))
        .map(|s| format!("{} (allow {}, deny {})", s.action, s.allow, s.deny))
        .collect();
    assert!(
        missing.is_empty(),
        "actions without allow+deny rows: {missing:?}"
    );
}

#[test]
fn rows_are_unique() {
    let mut seen = BTreeSet::new();
    for r in ROWS {
        let key = format!("{:?}|{:?}|{:?}", r.action, r.actor, r.fixture);
        assert!(seen.insert(key.clone()), "duplicate row {key}");
    }
}

#[test]
fn action_names_are_unique_and_all_listed() {
    let names: BTreeSet<&str> = Action::ALL.iter().map(|a| a.name()).collect();
    assert_eq!(names.len(), Action::ALL.len());
}

#[test]
fn anonymous_is_unauthenticated() {
    for &action in Action::ALL.iter().filter(|a| !a.is_public()) {
        let err = authorize(&Principal::anonymous(), action, &Resource::Instance)
            .expect_err("no anonymous access");
        assert_eq!(err.code(), ErrorCode::Unauthenticated, "{action:?}");
    }
    assert!(authorize(&Principal::anonymous(), Action::Health, &Resource::Instance).is_ok());
}

#[test]
fn disabled_account_is_denied() {
    let p = Principal::authenticated(ME, true, false, Credential::Session);
    let err = authorize(&p, Action::UserCreate, &Resource::Instance).expect_err("disabled");
    assert_eq!(err.code(), ErrorCode::AccountDisabled);
}

#[test]
fn invisible_private_project_is_not_found() {
    // Every action on a project-bearing resource gives the same 404 to a
    // non-member — no existence oracle.
    let invisible = project(None);
    let resources = [
        (Action::RepoRead, Resource::Project(invisible)),
        (Action::ProjectRead, Resource::Project(invisible)),
        (Action::ProjectUpdate, Resource::Project(invisible)),
        (Action::ProjectDelete, Resource::Project(invisible)),
        (Action::ProjectRestore, Resource::Project(invisible)),
        (
            Action::ProtectedRefManage,
            Resource::RefProtectionChange {
                project: invisible,
                from: None,
                to: Some(Role::Developer),
            },
        ),
        (
            Action::ProjectVisibilityChange,
            Resource::VisibilityChange {
                project: invisible,
                to: Visibility::Internal,
            },
        ),
        (Action::AuditRead, Resource::Project(invisible)),
        (
            Action::RepoPush,
            Resource::Ref {
                project: invisible,
                name: main_ref(),
                update: RefUpdate::FastForward,
                protection: None,
            },
        ),
        (
            Action::ProjectTransfer,
            Resource::Transfer {
                source: invisible,
                target_role: Some(Role::Owner),
            },
        ),
        (
            Action::MemberManage,
            Resource::Membership {
                scope: MembershipScope::Project(invisible),
                change: MembershipChange::Add(Role::Guest),
                removes_last_owner: false,
            },
        ),
    ];
    for (action, resource) in resources {
        let err = authorize(&session(), action, &resource).expect_err("not visible");
        assert_eq!(err.code(), ErrorCode::NotFound, "{action:?}");
    }
}

#[test]
fn pending_deletion_blocks_everything_but_restore() {
    let deleted = Resource::Project(ProjectView {
        pending_deletion: true,
        ..project(Some(Role::Owner))
    });
    for action in [
        Action::RepoRead,
        Action::ProjectRead,
        Action::ProjectUpdate,
        Action::ProjectDelete,
    ] {
        let err = authorize(&session(), action, &deleted).expect_err("pending deletion");
        assert_eq!(err.code(), ErrorCode::PendingDeletion, "{action:?}");
    }
    assert!(authorize(&session(), Action::ProjectRestore, &deleted).is_ok());
}

#[test]
fn token_scope_narrows_role() {
    // An Owner with a read_api token may read but not change anything.
    let p = Principal::authenticated(ME, false, true, Credential::Token(vec![Scope::ReadApi]));
    let owned = Resource::Project(project(Some(Role::Owner)));
    assert!(authorize(&p, Action::ProjectRead, &owned).is_ok());
    let err = authorize(&p, Action::ProjectDelete, &owned).expect_err("needs api scope");
    assert_eq!(err.code(), ErrorCode::InsufficientScope);
    let text = err.to_string();
    assert!(text.contains("required_scope=api"), "{text}");
    assert!(text.contains("token_scopes=read_api"), "{text}");
}

#[test]
fn scope_cannot_exceed_role() {
    let p = Principal::authenticated(
        ME,
        false,
        true,
        Credential::Token(vec![Scope::Api, Scope::WriteRepository]),
    );
    let err = authorize(
        &p,
        Action::ProjectUpdate,
        &Resource::Project(project(Some(Role::Reporter))),
    )
    .expect_err("role too low");
    assert_eq!(err.code(), ErrorCode::Forbidden);
}

#[test]
fn empty_scope_list_denies_everything_that_needs_a_scope() {
    let p = Principal::authenticated(ME, true, true, Credential::Token(vec![]));
    for &action in Action::ALL.iter().filter(|a| a.required_scope().is_some()) {
        let err = authorize(&p, action, &Resource::Instance).expect_err("no scope");
        assert!(
            err.code() == ErrorCode::InsufficientScope,
            "{action:?}: {:?}",
            err.code()
        );
    }
}

#[test]
fn admin_rights_through_a_token_need_the_admin_scope() {
    let api_only = Principal::authenticated(ME, true, true, Credential::Token(vec![Scope::Api]));
    let with_admin = Principal::authenticated(
        ME,
        true,
        true,
        Credential::Token(vec![Scope::Api, Scope::Admin]),
    );
    assert!(!api_only.admin_rights());
    assert!(with_admin.admin_rights());
    assert!(authorize(&api_only, Action::UserCreate, &Resource::Instance).is_err());
    assert!(authorize(&with_admin, Action::UserCreate, &Resource::Instance).is_ok());
}

#[test]
fn maintainer_cannot_demote_or_remove_an_owner() {
    let maintainer_scope = MembershipScope::Project(project(Some(Role::Maintainer)));
    for change in [
        MembershipChange::Change {
            from: Role::Owner,
            to: Role::Guest,
        },
        MembershipChange::Remove { from: Role::Owner },
        MembershipChange::Add(Role::Owner),
    ] {
        let err = authorize(
            &session(),
            Action::MemberManage,
            &Resource::Membership {
                scope: maintainer_scope,
                change,
                removes_last_owner: false,
            },
        )
        .expect_err("above own role");
        assert_eq!(err.code(), ErrorCode::Forbidden, "{change:?}");
    }
}

#[test]
fn a_token_cannot_mint_tokens() {
    for scopes in [
        vec![Scope::Api],
        vec![Scope::Api, Scope::Admin, Scope::WriteRepository],
    ] {
        let p = Principal::authenticated(ME, true, true, Credential::Token(scopes));
        let err = authorize(
            &p,
            Action::TokenCreate,
            &request(vec![Scope::ReadRepository]),
        )
        .expect_err("token cannot mint");
        assert_eq!(err.code(), ErrorCode::Forbidden);
    }
    assert!(authorize(&session(), Action::TokenCreate, &request(vec![Scope::Api])).is_ok());
}

fn request(scopes: Vec<Scope>) -> Resource {
    Resource::TokenRequest { owner: ME, scopes }
}

#[test]
fn only_admins_create_admin_scoped_tokens() {
    let err = authorize(
        &session(),
        Action::TokenCreate,
        &request(vec![Scope::Admin]),
    )
    .expect_err("not an admin");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(
        err.to_string()
            .contains("reason=admin_scope_requires_admin"),
        "{err}"
    );
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    assert!(authorize(&admin, Action::TokenCreate, &request(vec![Scope::Admin])).is_ok());
}

#[test]
fn maintainer_cannot_lift_or_set_an_owner_level_protection() {
    let maintainer = project(Some(Role::Maintainer));
    for (from, to) in [
        (Some(Role::Owner), None),
        (Some(Role::Owner), Some(Role::Developer)),
        (None, Some(Role::Owner)),
    ] {
        let err = authorize(
            &session(),
            Action::ProtectedRefManage,
            &Resource::RefProtectionChange {
                project: maintainer,
                from,
                to,
            },
        )
        .expect_err("above own role");
        assert_eq!(err.code(), ErrorCode::Forbidden, "{from:?} -> {to:?}");
        assert!(err.to_string().contains("reason=above_own_role"), "{err}");
    }
}

#[test]
fn visibility_change_needs_an_owner() {
    let change = |role| Resource::VisibilityChange {
        project: project(Some(role)),
        to: Visibility::Internal,
    };
    let err = authorize(
        &session(),
        Action::ProjectVisibilityChange,
        &change(Role::Maintainer),
    )
    .expect_err("maintainer");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(authorize(
        &session(),
        Action::ProjectVisibilityChange,
        &change(Role::Owner)
    )
    .is_ok());
}

#[test]
fn the_last_owner_cannot_be_removed_even_by_an_admin() {
    let last = Resource::Membership {
        scope: MembershipScope::Project(project(Some(Role::Owner))),
        change: MembershipChange::Remove { from: Role::Owner },
        removes_last_owner: true,
    };
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    for p in [session(), admin] {
        let err = authorize(&p, Action::MemberManage, &last).expect_err("last owner");
        assert!(err.to_string().contains("reason=last_owner"), "{err}");
    }
}

#[test]
fn ref_kind_is_derived_from_the_ref_name() {
    let kind = |n: &str| RefKind::of(&forge_core::RefName::parse(n).expect("valid ref"));
    assert_eq!(kind("refs/heads/main"), RefKind::Branch);
    assert_eq!(kind("refs/tags/v1.0"), RefKind::Tag);
    for reserved in [
        "refs/merge-requests/1/head",
        "refs/replace/0123456789abcdef0123456789abcdef01234567",
        "refs/meta/config",
        "refs/notes/commits",
    ] {
        assert_eq!(kind(reserved), RefKind::Reserved, "{reserved}");
    }
}

#[test]
fn reserved_ref_namespace_is_rejected_even_for_admins() {
    assert_eq!(
        decide(
            Action::RepoPush,
            Actor::Admin,
            Fixture::Ref {
                kind: RefKind::Reserved,
                update: RefUpdate::Create,
                protected: None
            }
        ),
        Expect::Deny(ErrorCode::ReservedRefNamespace)
    );
}

#[test]
fn force_push_to_protected_is_denied_even_for_admin() {
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    let err = authorize(
        &admin,
        Action::RepoPush,
        &Resource::Ref {
            project: project(None),
            name: main_ref(),
            update: RefUpdate::Force,
            protection: Some(RefProtection {
                push_min_role: Role::Owner,
            }),
        },
    )
    .expect_err("protected");
    assert_eq!(err.code(), ErrorCode::ProtectedRefForcePush);
    assert!(err.to_string().contains("protection=owner"), "{err}");
}

#[test]
fn push_below_protection_min_role_is_denied() {
    let err = authorize(
        &session(),
        Action::RepoPush,
        &Resource::Ref {
            project: project(Some(Role::Developer)),
            name: main_ref(),
            update: RefUpdate::FastForward,
            protection: Some(RefProtection {
                push_min_role: Role::Maintainer,
            }),
        },
    )
    .expect_err("below min role");
    assert_eq!(err.code(), ErrorCode::ProtectedRefPushNotAllowed);
    let text = err.to_string();
    for key in [
        "protection=maintainer",
        "required_role=maintainer",
        "actual_role=developer",
    ] {
        assert!(text.contains(key), "{key} missing: {text}");
    }
}

#[test]
fn delete_protected_ref_is_denied() {
    assert_eq!(
        decide(
            Action::RepoPush,
            Actor::Owner,
            Fixture::Ref {
                kind: RefKind::Branch,
                update: RefUpdate::Delete,
                protected: Some(Role::Developer)
            }
        ),
        Expect::Deny(ErrorCode::ProtectedRefDeletion)
    );
}

#[test]
fn mismatched_resource_is_an_internal_error_not_an_allow() {
    let admin = Principal::authenticated(ME, true, true, Credential::Session);
    for &action in Action::ALL.iter().filter(|a| !a.is_public()) {
        // A token resource fits only the token actions.
        if matches!(
            action,
            Action::TokenCreate | Action::TokenList | Action::TokenRevoke
        ) {
            continue;
        }
        let err =
            authorize(&admin, action, &Resource::Token { owner: ME }).expect_err("wrong shape");
        assert_eq!(err.code(), ErrorCode::Internal, "{action:?}");
    }
}

#[test]
fn denial_never_mentions_resource_names_or_paths() {
    let err = authorize(
        &session(),
        Action::ProjectUpdate,
        &Resource::Project(project(Some(Role::Developer))),
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

fn main_ref() -> forge_core::RefName {
    forge_core::RefName::parse("refs/heads/main").expect("valid ref")
}

#[test]
fn overlapping_protections_use_the_strictest_rule() {
    // Adding a weaker rule over an Owner-level one must not loosen it.
    let effective = RefProtection::effective([Role::Owner, Role::Developer]);
    assert_eq!(
        effective,
        Some(RefProtection {
            push_min_role: Role::Owner
        })
    );
    assert_eq!(RefProtection::effective([]), None);
    let err = authorize(
        &session(),
        Action::RepoPush,
        &Resource::Ref {
            project: project(Some(Role::Maintainer)),
            name: main_ref(),
            update: RefUpdate::FastForward,
            protection: RefProtection::effective([Role::Owner, Role::Developer]),
        },
    )
    .expect_err("strictest rule wins");
    assert_eq!(err.code(), ErrorCode::ProtectedRefPushNotAllowed);
}

#[test]
fn a_mislabelled_reserved_ref_is_still_rejected() {
    // The kind comes from the name inside authorize(), not from the caller.
    let replace =
        forge_core::RefName::parse("refs/replace/0123456789abcdef0123456789abcdef01234567")
            .expect("valid ref");
    let err = authorize(
        &Principal::authenticated(ME, true, true, Credential::Session),
        Action::RepoPush,
        &Resource::Ref {
            project: project(Some(Role::Owner)),
            name: replace,
            update: RefUpdate::Create,
            protection: None,
        },
    )
    .expect_err("reserved");
    assert_eq!(err.code(), ErrorCode::ReservedRefNamespace);
}
