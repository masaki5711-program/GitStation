use std::collections::BTreeSet;

use forge_errors::ErrorCode;

use crate::matrix::{summary, Actor, Expect, Fixture, ME, ROWS};
use crate::{
    authorize, Action, Credential, MembershipChange, MembershipScope, Principal, ProjectView,
    RefKind, RefProtection, RefUpdate, Resource, Role, Scope, TombstoneRelease, TransferTarget,
    Visibility,
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
                target: TransferTarget::Group {
                    role: Some(Role::Owner),
                    visible: true,
                },
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
        (
            Action::MemberManage,
            Resource::Membership {
                scope: MembershipScope::Project(invisible),
                change: MembershipChange::Missing { to: None },
                removes_last_owner: false,
            },
        ),
        (Action::MemberList, Resource::Project(invisible)),
        (Action::ProtectedRefList, Resource::Project(invisible)),
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
        Action::MemberList,
        Action::ProtectedRefList,
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
            protection: RefProtection::effective([Role::Owner]),
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
            protection: RefProtection::effective([Role::Maintainer]),
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
    assert_eq!(effective, RefProtection::effective([Role::Owner]));
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

/// M1 phase 8b MR7a: an agent's credential never changes visibility —
/// an Owner's or an administrator's alike, with `reason=agent_token` —
/// while the same token creates and updates (ADR-0017 amendment).
#[test]
fn an_agent_token_is_refused_the_visibility_change_only() {
    let make_internal = Resource::VisibilityChange {
        project: project(Some(Role::Owner)),
        to: Visibility::Internal,
    };
    for admin in [false, true] {
        let scopes = if admin {
            vec![Scope::Api, Scope::Admin]
        } else {
            vec![Scope::Api]
        };
        let agent = Principal::authenticated(ME, admin, true, Credential::Token(scopes.clone()))
            .as_agent(true);
        let err = authorize(&agent, Action::ProjectVisibilityChange, &make_internal)
            .expect_err("agents never change visibility");
        assert_eq!(err.code(), ErrorCode::Forbidden);
        assert!(err.to_string().contains("reason=agent_token"), "{err}");
        let human = Principal::authenticated(ME, admin, true, Credential::Token(scopes));
        assert!(authorize(&human, Action::ProjectVisibilityChange, &make_internal).is_ok());
        assert!(authorize(
            &agent,
            Action::ProjectUpdate,
            &Resource::Project(project(Some(Role::Owner)))
        )
        .is_ok());
    }
}

/// M1 phase 8b MR8: an agent's credential never manages members or
/// protections — a Maintainer's, an Owner's or an administrator's alike,
/// with `reason=agent_token` — while the same caller without `is_agent`
/// may, and the agent still reads the lists (ADR-0017 amendment MR8).
#[test]
fn an_agent_token_is_refused_member_and_protection_changes() {
    let add = |role| Resource::Membership {
        scope: MembershipScope::Project(project(Some(role))),
        change: MembershipChange::Add(Role::Developer),
        removes_last_owner: false,
    };
    let group_add = Resource::Membership {
        scope: MembershipScope::Group {
            role: Some(Role::Owner),
        },
        change: MembershipChange::Add(Role::Developer),
        removes_last_owner: false,
    };
    let lift = |role| Resource::RefProtectionChange {
        project: project(Some(role)),
        from: Some(Role::Maintainer),
        to: None,
    };
    for (admin, role) in [
        (false, Role::Maintainer),
        (false, Role::Owner),
        (true, Role::Owner),
    ] {
        let scopes = if admin {
            vec![Scope::Api, Scope::Admin]
        } else {
            vec![Scope::Api]
        };
        let human = Principal::authenticated(ME, admin, true, Credential::Token(scopes));
        let agent = human.clone().as_agent(true);
        for (action, resource) in [
            (Action::MemberManage, add(role)),
            (Action::MemberManage, group_add.clone()),
            (Action::ProtectedRefManage, lift(role)),
        ] {
            let err = authorize(&agent, action, &resource).expect_err("agents never");
            assert_eq!(err.code(), ErrorCode::Forbidden, "{action:?}");
            assert!(err.to_string().contains("reason=agent_token"), "{err}");
            assert!(authorize(&human, action, &resource).is_ok(), "{action:?}");
        }
        for action in [Action::MemberList, Action::ProtectedRefList] {
            assert!(authorize(&agent, action, &Resource::Project(project(Some(role)))).is_ok());
        }
    }
}

/// Nobody raises their own role: a Developer adding themself (or anyone)
/// as Owner needs Maintainer, a Maintainer promoting themself to Owner is
/// above their own role (M1 phase 8b MR8). `authorize()` does not need to
/// know who the member is: the role being granted is capped by the
/// caller's.
#[test]
fn self_elevation_is_denied() {
    let on = |caller: Role, change| Resource::Membership {
        scope: MembershipScope::Project(project(Some(caller))),
        change,
        removes_last_owner: false,
    };
    let err = authorize(
        &session(),
        Action::MemberManage,
        &on(Role::Developer, MembershipChange::Add(Role::Owner)),
    )
    .expect_err("developer");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(
        err.to_string().contains("required_role=maintainer"),
        "{err}"
    );
    let err = authorize(
        &session(),
        Action::MemberManage,
        &on(
            Role::Maintainer,
            MembershipChange::Change {
                from: Role::Maintainer,
                to: Role::Owner,
            },
        ),
    )
    .expect_err("maintainer to owner");
    assert!(err.to_string().contains("reason=above_own_role"), "{err}");
    // A user who is no member: the role asked for is still capped.
    let err = authorize(
        &session(),
        Action::MemberManage,
        &on(
            Role::Maintainer,
            MembershipChange::Missing {
                to: Some(Role::Owner),
            },
        ),
    )
    .expect_err("missing to owner");
    assert!(err.to_string().contains("reason=above_own_role"), "{err}");
}

fn admin_session() -> Principal {
    Principal::authenticated(ME, true, true, Credential::Session)
}

fn admin_token() -> Principal {
    Principal::authenticated(
        ME,
        true,
        true,
        Credential::Token(vec![Scope::Api, Scope::Admin]),
    )
}

fn reason_of(err: &forge_errors::ForgeError) -> String {
    err.to_string()
}

/// D5 (ADR-0015): a project lives under a namespace. Moving one to the top
/// level is refused for its Owner and for administrators (session or
/// `admin` token) alike, while the same administrator may move it into
/// any group or personal namespace.
#[test]
fn project_transfer_to_top_level_is_denied_for_admins() {
    let to = |target| Resource::Transfer {
        source: project(Some(Role::Owner)),
        target,
    };
    for caller in [session(), admin_session(), admin_token()] {
        let err = authorize(
            &caller,
            Action::ProjectTransfer,
            &to(TransferTarget::TopLevel),
        )
        .expect_err("never to the top level");
        assert_eq!(err.code(), ErrorCode::Forbidden);
        assert!(
            reason_of(&err).contains("reason=project_to_top_level"),
            "{err}"
        );
    }
    for target in [
        TransferTarget::Group {
            role: None,
            visible: true,
        },
        TransferTarget::PersonalNamespace {
            owner: crate::matrix::OTHER,
        },
    ] {
        assert!(authorize(&admin_session(), Action::ProjectTransfer, &to(target)).is_ok());
    }
    // A target that cannot be seen (or does not exist) is the same 404 for
    // everyone, administrators included — never a move to "no parent".
    let hidden = to(TransferTarget::Group {
        role: None,
        visible: false,
    });
    for caller in [session(), admin_session()] {
        let err = authorize(&caller, Action::ProjectTransfer, &hidden).expect_err("hidden");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(
            reason_of(&err).contains("reason=target_not_visible"),
            "{err}"
        );
    }
}

/// D5 (ADR-0015): a group moves only with Owner on the group and Owner on
/// the target (every member of the target inherits a role in it); to the
/// top level only by an administrator, never into a personal namespace.
#[test]
fn group_transfer_needs_owner_on_both_sides() {
    let group = |source: Role, target: Option<Role>| Resource::GroupTransfer {
        source_role: Some(source),
        target: TransferTarget::Group {
            role: target,
            visible: true,
        },
    };
    assert!(authorize(
        &session(),
        Action::GroupTransfer,
        &group(Role::Owner, Some(Role::Owner))
    )
    .is_ok());
    for (source, target) in [
        (Role::Maintainer, Some(Role::Owner)),
        (Role::Owner, Some(Role::Maintainer)),
        (Role::Owner, None),
    ] {
        let err = authorize(&session(), Action::GroupTransfer, &group(source, target))
            .expect_err("owner on both sides");
        assert_eq!(err.code(), ErrorCode::Forbidden, "{source:?} {target:?}");
        assert!(reason_of(&err).contains("required_role=owner"), "{err}");
    }
    let top = Resource::GroupTransfer {
        source_role: Some(Role::Owner),
        target: TransferTarget::TopLevel,
    };
    let err = authorize(&session(), Action::GroupTransfer, &top).expect_err("owner");
    assert!(reason_of(&err).contains("required_role=admin"), "{err}");
    assert!(authorize(&admin_session(), Action::GroupTransfer, &top).is_ok());
    let personal = Resource::GroupTransfer {
        source_role: Some(Role::Owner),
        target: TransferTarget::PersonalNamespace { owner: ME },
    };
    for caller in [session(), admin_session()] {
        let err = authorize(&caller, Action::GroupTransfer, &personal).expect_err("never");
        assert!(
            reason_of(&err).contains("reason=group_to_personal_namespace"),
            "{err}"
        );
    }
}

/// D5 (ADR-0015): a deleted tombstone is released by its recorded owner
/// only while that user is still Owner of the parent. A former owner —
/// demoted (403) or no longer a member (the same 404 as no tombstone) —
/// cannot; a disabled recorded owner is refused as disabled
/// (FG-AUTH-0004); without a parent, administrators only.
#[test]
fn former_owner_cannot_release_deleted_tombstone() {
    let deleted = |parent_role: Option<Role>| {
        Resource::Tombstone(TombstoneRelease::Deleted {
            recorded_owner: Some(ME),
            caller_parent_role: parent_role,
            has_parent: true,
        })
    };
    let release = Action::NamespaceTombstoneRelease;
    assert!(authorize(&session(), release, &deleted(Some(Role::Owner))).is_ok());
    let err = authorize(&session(), release, &deleted(Some(Role::Maintainer)))
        .expect_err("demoted former owner");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(
        reason_of(&err).contains("reason=not_recorded_owner"),
        "{err}"
    );
    let err = authorize(&session(), release, &deleted(None)).expect_err("left the parent");
    assert_eq!(err.code(), ErrorCode::NotFound);
    let disabled = Principal::authenticated(ME, false, false, Credential::Session);
    let err = authorize(&disabled, release, &deleted(Some(Role::Owner))).expect_err("disabled");
    assert_eq!(err.code(), ErrorCode::AccountDisabled);
    // Another Owner of the parent is not the recorded owner.
    let foreign = Resource::Tombstone(TombstoneRelease::Deleted {
        recorded_owner: Some(crate::matrix::OTHER),
        caller_parent_role: Some(Role::Owner),
        has_parent: true,
    });
    assert_eq!(
        authorize(&session(), release, &foreign)
            .expect_err("not recorded")
            .code(),
        ErrorCode::Forbidden
    );
    assert!(authorize(&admin_session(), release, &foreign).is_ok());
    let top = Resource::Tombstone(TombstoneRelease::Deleted {
        recorded_owner: Some(ME),
        caller_parent_role: None,
        has_parent: false,
    });
    let err = authorize(&session(), release, &top).expect_err("no parent: admins only");
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert!(
        reason_of(&err).contains("reason=tombstone_not_visible"),
        "{err}"
    );
    // An agent gets the same 404 as a person on a tombstone it cannot see.
    let agent = Principal::authenticated(ME, false, true, Credential::Token(vec![Scope::Api]))
        .as_agent(true);
    assert_eq!(
        authorize(&agent, release, &deleted(None))
            .expect_err("agent")
            .code(),
        ErrorCode::NotFound
    );
    assert!(authorize(&admin_session(), release, &top).is_ok());
    // A moved tombstone: its target's current Owner takes it back.
    let moved = |role| {
        Resource::Tombstone(TombstoneRelease::Moved {
            target_owner_role: role,
            target_visible: true,
        })
    };
    assert!(authorize(&session(), release, &moved(Some(Role::Owner))).is_ok());
    assert_eq!(
        authorize(&session(), release, &moved(Some(Role::Maintainer)))
            .expect_err("maintainer")
            .code(),
        ErrorCode::Forbidden
    );
}

/// D5: an agent's credential never deletes, restores, moves, renames or
/// releases — an Owner's or an administrator's alike, with
/// `reason=agent_token` — while the same caller without `is_agent` may.
#[test]
fn agent_token_cannot_delete_transfer_or_release() {
    let owned = project(Some(Role::Owner));
    let cases = [
        (Action::ProjectDelete, Resource::Project(owned)),
        (
            Action::ProjectRestore,
            Resource::Project(ProjectView {
                pending_deletion: true,
                ..owned
            }),
        ),
        (
            Action::ProjectTransfer,
            Resource::Transfer {
                source: owned,
                target: TransferTarget::Group {
                    role: Some(Role::Owner),
                    visible: true,
                },
            },
        ),
        (
            Action::GroupTransfer,
            Resource::GroupTransfer {
                source_role: Some(Role::Owner),
                target: TransferTarget::Group {
                    role: Some(Role::Owner),
                    visible: true,
                },
            },
        ),
        (
            Action::NamespaceTombstoneRelease,
            Resource::Tombstone(TombstoneRelease::Moved {
                target_owner_role: Some(Role::Owner),
                target_visible: true,
            }),
        ),
        (
            Action::NamespaceTombstoneRelease,
            Resource::Tombstone(TombstoneRelease::Deleted {
                recorded_owner: Some(ME),
                caller_parent_role: Some(Role::Owner),
                has_parent: true,
            }),
        ),
    ];
    let rename = (
        Action::UserRename,
        Resource::UserRename {
            target_user: crate::matrix::OTHER,
        },
    );
    for admin in [false, true] {
        let scopes = if admin {
            vec![Scope::Api, Scope::Admin]
        } else {
            vec![Scope::Api]
        };
        let human = Principal::authenticated(ME, admin, true, Credential::Token(scopes));
        let agent = human.clone().as_agent(true);
        let all = cases.iter().chain(admin.then_some(&rename));
        for (action, resource) in all {
            let err = authorize(&agent, *action, resource).expect_err("agents never");
            assert_eq!(err.code(), ErrorCode::Forbidden, "{action:?}");
            assert!(reason_of(&err).contains("reason=agent_token"), "{err}");
            assert!(authorize(&human, *action, resource).is_ok(), "{action:?}");
        }
    }
}
