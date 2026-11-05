//! `authorize()` on discussions (M3 R3, ADR-0024 §8.4, §12): the denial
//! reasons the matrix rows cannot show, the account kind's people-only
//! arm on the older people-only actions, the invisible project's 404 and
//! the proof's id and reconfirmation.

use forge_core::{DiscussionId, MergeRequestState, ProjectId, RefName, UserId};
use forge_errors::{ErrorCode, ForgeError};

use crate::matrix::{ME, OTHER};
use crate::{
    authorize, prove, AccountFacts, AccountKind, Action, Credential, DiscussionCtx,
    LoadedDiscussion, LoadedMergeRequest, LoadedProject, Principal, ProjectView, Resource,
    ResourceCtx, Role, Scope, Visibility,
};

fn token(kind: AccountKind) -> Principal {
    Principal::authenticated(ME, false, true, Credential::Token(vec![Scope::Api]), kind)
}

fn session() -> Principal {
    Principal::authenticated(ME, false, true, Credential::Session, AccountKind::Human)
}

fn thread(
    role: Option<Role>,
    visibility: Visibility,
    mr_author: UserId,
    started_by: UserId,
) -> Resource {
    Resource::Discussion {
        project: ProjectView {
            visibility,
            role,
            pending_deletion: false,
        },
        mr_author,
        started_by,
    }
}

fn reason(err: &ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

#[test]
fn resolving_is_kept_for_people_with_the_reason_of_the_caller_kind() {
    let mine = thread(Some(Role::Maintainer), Visibility::Private, OTHER, ME);
    for (principal, why) in [
        (token(AccountKind::Human).as_agent(true), "agent_token"),
        (token(AccountKind::ServiceAccount), "service_account"),
        (token(AccountKind::Bot), "bot"),
    ] {
        let err = authorize(&principal, Action::DiscussionResolve, &mine).expect_err(why);
        assert_eq!(err.code(), ErrorCode::Forbidden, "{why}");
        assert_eq!(reason(&err), why);
    }
    assert!(authorize(&token(AccountKind::Human), Action::DiscussionResolve, &mine).is_ok());
}

#[test]
fn the_merge_request_author_never_resolves_another_s_thread() {
    let theirs_on_mine = thread(Some(Role::Owner), Visibility::Private, ME, OTHER);
    let err =
        authorize(&session(), Action::DiscussionResolve, &theirs_on_mine).expect_err("author");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(reason(&err), "author_foreign_discussion");
    // Administrators too.
    let admin = Principal::authenticated(ME, true, true, Credential::Session, AccountKind::Human);
    let err = authorize(
        &admin,
        Action::DiscussionResolve,
        &thread(None, Visibility::Private, ME, OTHER),
    )
    .expect_err("admin author");
    assert_eq!(reason(&err), "author_foreign_discussion");
    // But the author's own thread is theirs to resolve.
    let mine_on_mine = thread(Some(Role::Developer), Visibility::Private, ME, ME);
    assert!(authorize(&session(), Action::DiscussionResolve, &mine_on_mine).is_ok());
}

#[test]
fn an_invisible_project_s_discussion_is_not_found_like_a_missing_one() {
    let invisible = thread(None, Visibility::Private, OTHER, ME);
    let err = authorize(&session(), Action::DiscussionResolve, &invisible).expect_err("invisible");
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert_eq!(reason(&err), "not_visible");
}

/// ADR-0024 §8.4: the account kind joined every action kept for people,
/// so a service account's or a bot's token is refused there too (fail
/// closed; before M3 R3 the kind was no input).
#[test]
fn service_accounts_and_bots_are_refused_the_older_people_only_actions() {
    let project = ProjectView {
        visibility: Visibility::Private,
        role: Some(Role::Owner),
        pending_deletion: false,
    };
    let change = Resource::VisibilityChange {
        project,
        to: Visibility::Internal,
    };
    for (kind, why) in [
        (AccountKind::ServiceAccount, "service_account"),
        (AccountKind::Bot, "bot"),
    ] {
        let err = authorize(&token(kind), Action::ProjectVisibilityChange, &change).expect_err(why);
        assert_eq!(reason(&err), why);
    }
    assert!(authorize(
        &token(AccountKind::Human),
        Action::ProjectVisibilityChange,
        &change
    )
    .is_ok());
}

fn ctx(started_by: UserId) -> DiscussionCtx {
    DiscussionCtx::new(
        LoadedProject {
            id: ProjectId::new(4),
            visibility: Visibility::Private,
            role: Some(Role::Reporter),
            pending_deletion: false,
        },
        LoadedMergeRequest {
            iid: Some(9),
            author: OTHER,
            state: MergeRequestState::Open,
            draft: false,
            source_branch: RefName::parse("refs/heads/feature").expect("ref"),
            target_branch: RefName::parse("refs/heads/main").expect("ref"),
        },
        LoadedDiscussion {
            id: DiscussionId::new(31),
            started_by,
        },
    )
}

#[test]
fn a_discussion_proof_names_its_thread_and_reconfirms_with_the_same_kind() {
    let proof = prove(&session(), Action::DiscussionResolve, ctx(ME)).expect("own thread");
    assert_eq!(
        proof.id(),
        &(ProjectId::new(4), Some(9), DiscussionId::new(31))
    );
    assert_eq!(ctx(ME).resource(), proof.ctx().resource());
    let facts = AccountFacts {
        active: true,
        is_admin: false,
    };
    // Another starter seen under the lock: the Reporter is refused now.
    let err = proof.reconfirm(facts, ctx(OTHER)).expect_err("not theirs");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    // The account kind survives the reconfirmation (it never changes).
    let sa = token(AccountKind::ServiceAccount);
    let err = prove(&sa, Action::DiscussionResolve, ctx(ME)).expect_err("service account");
    assert_eq!(reason(&err), "service_account");
    assert_eq!(proof.principal().account_kind(), AccountKind::Human);
}

/// ADR-0024 §8.4 (M3 R3 review): every action kept for people has a
/// service account AND a bot deny row in the matrix.
#[test]
fn every_people_only_action_denies_service_accounts_and_bots() {
    use crate::matrix::{Expect, ROWS};
    let mut missing = Vec::new();
    for &action in Action::ALL.iter().filter(|a| a.is_people_only()) {
        for kind in [AccountKind::ServiceAccount, AccountKind::Bot] {
            let found = ROWS.iter().any(|r| {
                r.action == action && r.actor.kind() == kind && matches!(r.expect, Expect::Deny(_))
            });
            if !found {
                missing.push(format!("{} × {}", action.name(), kind.as_str()));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "people-only actions without deny rows: {missing:?}"
    );
}

/// Each service account / bot deny row of a people-only action is allowed
/// for the person with the same role and scopes, and denied with the
/// kind's own reason: the denial comes from the account kind alone, so
/// removing `people_only` turns the matrix red.
#[test]
fn people_only_rows_are_allowed_for_the_human_twin() {
    use crate::matrix::{Expect, ROWS};
    let mut checked = 0;
    for r in ROWS.iter() {
        let Some(twin) = r.actor.human_twin() else {
            continue;
        };
        if !r.action.is_people_only() || r.expect != Expect::Deny(ErrorCode::Forbidden) {
            continue;
        }
        let denied = authorize(&r.actor.principal(), r.action, &r.fixture.resource(r.actor))
            .expect_err("a deny row");
        assert_eq!(reason(&denied), r.actor.kind().as_str(), "{r:?}");
        assert!(
            authorize(&twin.principal(), r.action, &r.fixture.resource(twin)).is_ok(),
            "{:?} {:?}: the human twin {twin:?} must be allowed",
            r.action,
            r.fixture
        );
        checked += 1;
    }
    assert!(
        checked >= 26,
        "every people-only action × 2 kinds ({checked})"
    );
}

/// The reverse direction: on every allowed row of an action not kept for
/// people, a service account or a bot with the same role and scopes is
/// allowed too (the kind decides nothing there; commenting stays open) —
/// except where M3 S0 made the kind decide on purpose, each with its own
/// log reason: a service account sees member projects only
/// (`service_account_not_member`, FG-REPO-0005), and a service account
/// that is a Guest of an internal project reads by its Guest role. (A
/// non-person's personal namespace is refused by its owner's kind, a
/// fixture fact: rows in `matrix_rows_service_accounts.rs`.)
/// The actions a person reads an internal project with as a Reporter
/// whatever their role (`read_role`): the only ones where a service
/// account's Guest role decides differently.
const INTERNAL_READS: &[Action] = &[
    Action::RepoRead,
    Action::RepoBrowse,
    Action::ProjectRead,
    Action::MemberList,
    Action::ProtectedRefList,
    Action::MergeRequestList,
    Action::MergeRequestRead,
];

/// The resource is (or belongs to) an internal project.
fn internal_project(resource: &crate::Resource) -> bool {
    use crate::{NamespaceView, Resource, Visibility};
    let project = match resource {
        Resource::Project(p) | Resource::MergeRequest { project: p, .. } => Some(p),
        Resource::Resolved {
            view: NamespaceView::Project(p),
            ..
        } => Some(p),
        Resource::Instance
        | Resource::Group { .. }
        | Resource::Namespace { .. }
        | Resource::Ref { .. }
        | Resource::Transfer { .. }
        | Resource::GroupTransfer { .. }
        | Resource::UserRename { .. }
        | Resource::Tombstone(_)
        | Resource::Membership { .. }
        | Resource::RefProtectionChange { .. }
        | Resource::VisibilityChange { .. }
        | Resource::Token { .. }
        | Resource::Session { .. }
        | Resource::TokenRequest { .. }
        | Resource::Admission { .. }
        | Resource::Unresolved(_)
        | Resource::Discussion { .. }
        | Resource::CodeownersEnforcement { .. }
        | Resource::MergeSettingsChange { .. }
        | Resource::Resolved { .. }
        | Resource::ServiceAccount(_)
        | Resource::ServiceAccountTokenRequest { .. }
        | Resource::CheckRun { .. }
        | Resource::IntegrationFeed(_)
        | Resource::ProjectEvents { .. }
        | Resource::ActAs(_)
        | Resource::ActAsConfirm(_) => None,
    };
    project.is_some_and(|p| p.visibility == Visibility::Internal)
}

#[test]
fn only_people_only_actions_refuse_by_account_kind() {
    use crate::matrix::{Expect, ROWS};
    let mut checked = 0;
    let mut by_design = 0;
    let mut unexpected = Vec::new();
    for r in ROWS
        .iter()
        .filter(|r| !r.action.is_people_only() && r.expect == Expect::Allow)
    {
        for kind in [AccountKind::ServiceAccount, AccountKind::Bot] {
            let principal = r.actor.principal().with_kind(kind);
            let resource = r.fixture.resource(r.actor);
            match authorize(&principal, r.action, &resource) {
                Ok(()) => checked += 1,
                Err(e) => {
                    let why = reason(&e);
                    let designed = match (kind, why.as_str()) {
                        (AccountKind::ServiceAccount, "service_account_not_member") => {
                            e.code() == ErrorCode::NotFound
                        }
                        // M3 C1 (ADR-0024 §10): a check run is reported by
                        // a service account only — the allowed rows are
                        // service accounts', a bot twin is refused.
                        (AccountKind::Bot, "not_service_account") => {
                            e.code() == ErrorCode::Forbidden
                                && matches!(
                                    r.action,
                                    Action::CheckWrite
                                        | Action::IntegrationEventRead
                                        | Action::IntegrationEventCursorRead
                                        | Action::IntegrationDevStatusRead
                                )
                        }
                        // M3 I1c (ADR-0016 §7): the feed is read by the
                        // integration's own service account only; a bot
                        // twin is the same 404 as another integration.
                        (AccountKind::Bot, "not_integration_account") => {
                            e.code() == ErrorCode::NotFound
                                && matches!(
                                    r.action,
                                    Action::IntegrationEventRead
                                        | Action::IntegrationEventCursorRead
                                        | Action::IntegrationDevStatusRead
                                        | Action::ActAsAssertionClaim
                                )
                        }
                        // M3 S2 (ADR-0025 §1): an act-as request acts for
                        // a person only (the resolution's not_human).
                        (
                            AccountKind::ServiceAccount | AccountKind::Bot,
                            "act_as_target_not_human",
                        ) => {
                            e.code() == ErrorCode::Forbidden
                                && matches!(r.actor, crate::matrix::Actor::ActAs(_))
                        }
                        // A Guest member of an internal project reads by
                        // its own role (a person reads it as a Reporter):
                        // only the read actions, only on an internal
                        // project, only for the Guest role (S0 review).
                        (AccountKind::ServiceAccount, "") => {
                            e.code() == ErrorCode::Forbidden
                                && INTERNAL_READS.contains(&r.action)
                                && internal_project(&resource)
                                && e.context()
                                    .iter()
                                    .any(|(k, v)| *k == "actual_role" && v == "guest")
                        }
                        _ => false,
                    };
                    if !designed {
                        unexpected.push(format!(
                            "{:?} {:?} {:?} refused for {} ({why})",
                            r.action,
                            r.actor,
                            r.fixture,
                            kind.as_str()
                        ));
                    }
                    by_design += 1;
                }
            }
        }
    }
    assert!(unexpected.is_empty(), "{}", unexpected.join("; "));
    assert!(checked > 100, "{checked}");
    assert!(by_design > 0, "the M3 S0 rules are exercised");
}
