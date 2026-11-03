//! `prove_visible` (M3 I1c, ADR-0016 §7): the feed's visible set is what
//! `authorize()` allowed for the integration class, per bound project,
//! narrowed by the allowlist; the binding generation is part of it.

use forge_core::{
    FeedBinding, FeedMaterial, IntegrationId, ProjectAccess, ProjectId, Role, UserId, Visibility,
};
use forge_errors::ErrorCode;

use crate::{
    prove, prove_visible, AccountKind, Action, Credential, IntegrationFeedCtx,
    LoadedIntegrationFeed, Principal, Scope,
};

const INTEGRATION: IntegrationId = IntegrationId::new(3);
const SA: UserId = UserId::new(40);

fn sa_token(scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        SA,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::ServiceAccount,
    )
}

fn loaded(enabled: bool) -> LoadedIntegrationFeed {
    LoadedIntegrationFeed {
        integration: INTEGRATION,
        service_account: SA,
        enabled,
    }
}

fn proof(action: Action) -> crate::Authorized<IntegrationFeedCtx> {
    prove(
        &sa_token(&[Scope::ReadApi]),
        action,
        IntegrationFeedCtx::new(loaded(true), Some(SA), false),
    )
    .expect("the integration's own service account reads its feed")
}

fn binding(project: i64, generation: i64, role: Option<Role>, deleted: bool) -> FeedBinding {
    FeedBinding {
        project: ProjectId::new(project),
        generation,
        access: Some(ProjectAccess {
            visibility: Visibility::Private,
            role,
            pending_deletion: deleted,
        }),
    }
}

fn material(bindings: Vec<FeedBinding>) -> FeedMaterial {
    FeedMaterial {
        integration: INTEGRATION,
        service_account: SA,
        enabled: true,
        event_types: vec!["*".to_owned()],
        bindings,
    }
}

#[test]
fn only_reporter_bindings_are_visible() {
    let set = prove_visible(
        &proof(Action::IntegrationEventRead),
        &material(vec![
            binding(1, 10, Some(Role::Reporter), false),
            binding(2, 11, Some(Role::Guest), false),
            binding(3, 12, None, false),
            binding(4, 13, Some(Role::Developer), true),
            binding(5, 14, Some(Role::Maintainer), false),
            FeedBinding {
                project: ProjectId::new(6),
                generation: 15,
                access: None,
            },
        ]),
    )
    .expect("decided");
    assert_eq!(set.project_ids(), [1, 5]);
    assert!(set.event_type_names().contains(&"merge_request.closed"));
    assert!(!set.event_type_names().contains(&"member.added"));
}

/// The cursor route decides the same set (so its head cursor continues
/// on the events route).
#[test]
fn both_feed_actions_decide_the_same_set() {
    let m = material(vec![binding(1, 10, Some(Role::Reporter), false)]);
    let read = prove_visible(&proof(Action::IntegrationEventRead), &m).expect("read");
    let head = prove_visible(&proof(Action::IntegrationEventCursorRead), &m).expect("head");
    assert_eq!(read.digest(), head.digest());
}

/// ADR-0016 Amendment I1c: an unbind and a rebind of the same project is a
/// new generation, so the set's digest changes (410 visible_set_changed).
#[test]
fn a_rebind_changes_the_visible_set_digest() {
    let p = proof(Action::IntegrationEventRead);
    let before = prove_visible(
        &p,
        &material(vec![binding(1, 10, Some(Role::Reporter), false)]),
    )
    .expect("before");
    let after = prove_visible(
        &p,
        &material(vec![binding(1, 11, Some(Role::Reporter), false)]),
    )
    .expect("after");
    assert_eq!(before.project_ids(), after.project_ids());
    assert_ne!(before.digest(), after.digest());
}

#[test]
fn a_disabled_integration_in_the_snapshot_is_refused() {
    let mut m = material(vec![binding(1, 10, Some(Role::Reporter), false)]);
    m.enabled = false;
    let err = prove_visible(&proof(Action::IntegrationEventRead), &m).expect_err("disabled");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "integration_disabled"));
}

#[test]
fn material_of_another_integration_is_an_internal_error() {
    let mut m = material(Vec::new());
    m.integration = IntegrationId::new(4);
    let err = prove_visible(&proof(Action::IntegrationEventRead), &m).expect_err("mismatch");
    assert_eq!(err.code(), ErrorCode::Internal);
}

#[test]
fn the_route_refuses_everyone_but_the_own_service_account() {
    let human = Principal::authenticated(SA, true, true, Credential::Session, AccountKind::Human);
    let err = prove(
        &human,
        Action::IntegrationEventRead,
        IntegrationFeedCtx::new(loaded(true), Some(SA), false),
    )
    .expect_err("a person");
    assert_eq!(err.code(), ErrorCode::NotFound);
    let err = prove(
        &sa_token(&[Scope::ReadApi]),
        Action::IntegrationEventRead,
        IntegrationFeedCtx::new(loaded(true), Some(UserId::new(41)), false),
    )
    .expect_err("another account");
    assert_eq!(err.code(), ErrorCode::NotFound);
    let err = prove(
        &sa_token(&[Scope::ReadApi]),
        Action::IntegrationEventRead,
        IntegrationFeedCtx::new(loaded(true), Some(SA), true),
    )
    .expect_err("act-as");
    assert_eq!(err.code(), ErrorCode::Forbidden);
}
