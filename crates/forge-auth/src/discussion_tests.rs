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
