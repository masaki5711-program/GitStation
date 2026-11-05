//! `prove_webhook_delivery` (M3 I2, ADR-0016 §8): a webhook delivers only
//! what its owner may still see as a Maintainer of the project, narrowed by
//! its allowlist; losing the project is `FG-INT-0010 reason=owner_lost`.

use forge_core::{
    EventType, ProjectAccess, ProjectId, Role, UserId, UserKind, Visibility, WebhookId,
    WebhookMaterial,
};
use forge_errors::ErrorCode;

use crate::prove_webhook_delivery;

fn material(role: Option<Role>) -> WebhookMaterial {
    WebhookMaterial {
        webhook: WebhookId::new(3),
        project: ProjectId::new(7),
        enabled: true,
        event_types: vec!["merge_request.*".to_owned(), "member.added".to_owned()],
        owner: UserId::new(5),
        owner_active: true,
        owner_admin: false,
        owner_kind: UserKind::Human,
        access: Some(ProjectAccess {
            visibility: Visibility::Private,
            role,
            pending_deletion: false,
        }),
    }
}

fn reason(e: &forge_errors::ForgeError) -> Option<&str> {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.as_str())
}

#[test]
fn a_maintainer_owner_gets_the_named_maintainer_type_and_the_integration_family() {
    let set = prove_webhook_delivery(&material(Some(Role::Maintainer))).expect("allowed");
    assert_eq!(set.webhook(), WebhookId::new(3));
    assert_eq!(set.project(), ProjectId::new(7));
    assert!(set.admits(EventType::MergeRequestMerged));
    assert!(set.admits(EventType::MemberAdded));
    assert!(!set.admits(EventType::MemberRemoved));
    assert!(!set.admits(EventType::RepositoryPushed));
    assert!(!set.admits(EventType::TokenCreated));
    let owner = prove_webhook_delivery(&material(Some(Role::Owner))).expect("owner");
    assert!(owner.admits(EventType::MemberAdded));
}

#[test]
fn an_owner_who_lost_the_project_is_int_0010_owner_lost() {
    let mut cases = vec![
        ("developer", material(Some(Role::Developer))),
        ("no member", material(None)),
    ];
    let mut disabled = material(Some(Role::Maintainer));
    disabled.owner_active = false;
    cases.push(("disabled", disabled));
    let mut deleted = material(Some(Role::Maintainer));
    if let Some(a) = deleted.access.as_mut() {
        a.pending_deletion = true;
    }
    cases.push(("pending deletion", deleted));
    let mut gone = material(Some(Role::Maintainer));
    gone.access = None;
    cases.push(("project gone", gone));
    let mut service_account = material(Some(Role::Maintainer));
    service_account.owner_kind = UserKind::ServiceAccount;
    cases.push(("service account", service_account));
    for (name, m) in cases {
        let e = prove_webhook_delivery(&m).expect_err(name);
        assert_eq!(e.code(), ErrorCode::WebhookTargetForbidden, "{name}");
        assert_eq!(reason(&e), Some("owner_lost"), "{name}");
    }
}

#[test]
fn an_administrator_owner_keeps_delivering_but_never_the_admin_class() {
    let mut m = material(None);
    m.owner_admin = true;
    m.event_types = vec!["*".to_owned(), "token.created".to_owned()];
    let set = prove_webhook_delivery(&m).expect("administrator");
    assert!(set.admits(EventType::MergeRequestOpened));
    assert!(!set.admits(EventType::TokenCreated));
}
