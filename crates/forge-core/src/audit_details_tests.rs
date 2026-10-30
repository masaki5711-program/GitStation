use serde_json::{json, Value};

use crate::ids::{NamespaceId, UserId};
use crate::{AuditDetails, Role, UserKind};

const PII_KEYS: &[&str] = &[
    "email",
    "employee_number",
    "external_id",
    "display_name",
    "username",
    "name",
    "ip",
    "source_ip",
    "user_agent",
    "actor_label",
];

fn keys(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                out.push(k.to_ascii_lowercase());
                keys(v, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| keys(v, out)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[test]
fn audit_details_have_no_pii_fields() {
    let created = AuditDetails::UserCreated {
        kind: UserKind::Human,
        namespace_id: NamespaceId::new(5),
        is_admin: false,
    };
    let disabled = AuditDetails::UserDisabled {
        kind: UserKind::ServiceAccount,
        revoked_tokens: 3,
        forced: true,
        ownerless_groups: 2,
        left_without_admin: false,
    };
    // D3 MR2: the recovery path writes ids and flags only.
    let bootstrapped = AuditDetails::AdminBootstrapped {
        created: true,
        namespace_id: Some(NamespaceId::new(8)),
    };
    let promoted = AuditDetails::AdminBootstrapped {
        created: false,
        namespace_id: None,
    };
    // The exact shape (a Null or an empty object would pass a key scan).
    assert_eq!(
        created.to_value(),
        json!({"kind": "human", "namespace_id": 5, "is_admin": false})
    );
    assert_eq!(
        disabled.to_value(),
        json!({
            "kind": "service_account",
            "revoked_tokens": 3,
            "forced": true,
            "ownerless_groups": 2,
            "left_without_admin": false,
        })
    );
    assert_eq!(
        bootstrapped.to_value(),
        json!({"created": true, "namespace_id": 8})
    );
    assert_eq!(
        promoted.to_value(),
        json!({"created": false, "namespace_id": null})
    );
    assert_eq!(bootstrapped.action(), "user.admin_bootstrapped");
    assert_eq!(created.action(), "user.created");
    assert_eq!(disabled.action(), "user.disabled");
    // D2: groups and their members (ADR-0016 §9: group.*, member.*).
    let group = AuditDetails::GroupCreated {
        namespace_id: NamespaceId::new(7),
        parent_namespace_id: Some(NamespaceId::new(3)),
        owner: UserId::new(11),
    };
    let added = AuditDetails::MemberAdded {
        user: UserId::new(12),
        role: Role::Developer,
    };
    let changed = AuditDetails::MemberRoleChanged {
        user: UserId::new(12),
        from: Role::Developer,
        to: Role::Maintainer,
    };
    let removed = AuditDetails::MemberRemoved {
        user: UserId::new(12),
        role: Role::Maintainer,
    };
    assert_eq!(
        group.to_value(),
        json!({"namespace_id": 7, "parent_namespace_id": 3, "owner_id": 11})
    );
    assert_eq!(
        AuditDetails::GroupCreated {
            namespace_id: NamespaceId::new(7),
            parent_namespace_id: None,
            owner: UserId::new(11),
        }
        .to_value(),
        json!({"namespace_id": 7, "parent_namespace_id": null, "owner_id": 11})
    );
    assert_eq!(
        added.to_value(),
        json!({"user_id": 12, "role": "developer"})
    );
    assert_eq!(
        changed.to_value(),
        json!({"user_id": 12, "from": "developer", "to": "maintainer"})
    );
    assert_eq!(
        removed.to_value(),
        json!({"user_id": 12, "role": "maintainer"})
    );
    assert_eq!(group.action(), "group.created");
    assert_eq!(added.action(), "member.added");
    assert_eq!(changed.action(), "member.role_changed");
    assert_eq!(removed.action(), "member.removed");
    for details in [
        created,
        disabled,
        bootstrapped,
        promoted,
        group,
        added,
        changed,
        removed,
    ] {
        let mut found = Vec::new();
        keys(&details.to_value(), &mut found);
        for pii in PII_KEYS {
            assert!(!found.iter().any(|k| k == pii), "{pii} in {details:?}");
        }
    }
}

#[test]
fn token_audit_details_have_their_exact_shape_and_no_secret() {
    let created = AuditDetails::TokenCreated {
        token_id: 9,
        user: UserId::new(4),
        public_id: "0123456789abcdef".to_owned(),
        scopes: vec![crate::Scope::ReadApi, crate::Scope::WriteRepository],
        expires_at: "2027-01-28T00:00:00Z".to_owned(),
        is_agent: true,
    };
    let revoked = AuditDetails::TokenRevoked {
        token_id: 9,
        user: UserId::new(4),
        public_id: "0123456789abcdef".to_owned(),
    };
    assert_eq!(
        created.to_value(),
        json!({
            "token_id": 9,
            "user_id": 4,
            "public_id": "0123456789abcdef",
            "scopes": ["read_api", "write_repository"],
            "expires_at": "2027-01-28T00:00:00Z",
            "is_agent": true,
        })
    );
    assert_eq!(
        revoked.to_value(),
        json!({"token_id": 9, "user_id": 4, "public_id": "0123456789abcdef"})
    );
    assert_eq!(created.action(), "token.created");
    assert_eq!(revoked.action(), "token.revoked");
    for value in [created.to_value(), revoked.to_value()] {
        let mut found = Vec::new();
        keys(&value, &mut found);
        for k in &found {
            assert!(
                !k.contains("secret") && !k.contains("hash") && !PII_KEYS.contains(&k.as_str()),
                "{k}"
            );
        }
    }
}

#[test]
fn project_created_has_ids_kind_and_visibility_only_and_publishes_its_event() {
    use crate::ids::{ProjectId, RepoId};
    use crate::{EventType, RepositoryKind, Visibility};
    let created = AuditDetails::ProjectCreated {
        project_id: ProjectId::new(21),
        namespace_id: NamespaceId::new(8),
        parent_namespace_id: NamespaceId::new(3),
        repository_id: RepoId::new(34),
        kind: RepositoryKind::System,
        visibility: Visibility::Private,
        owner: UserId::new(5),
    };
    assert_eq!(
        created.to_value(),
        json!({
            "project_id": 21,
            "namespace_id": 8,
            "parent_namespace_id": 3,
            "repository_id": 34,
            "kind": "system",
            "visibility": "private",
            "owner_id": 5,
        })
    );
    assert_eq!(created.action(), "project.created");
    // The one D4 integration event; its name is the audit action.
    assert_eq!(created.event_type(), Some(EventType::ProjectCreated));
    assert_eq!(
        created.event_type().map(EventType::as_str),
        Some(created.action())
    );
    let mut found = Vec::new();
    keys(&created.to_value(), &mut found);
    for pii in PII_KEYS {
        assert!(!found.iter().any(|k| k == pii), "{pii}");
    }
}

// ── D4 MR3b: repository provisioning ──
#[test]
fn repository_created_and_reprovision_requested_have_ids_only() {
    use crate::ids::{ProjectId, RepoId};
    use crate::{EventType, RepositoryKind};
    let created = AuditDetails::RepositoryCreated {
        project_id: ProjectId::new(21),
        repository_id: RepoId::new(34),
        kind: RepositoryKind::Spec,
    };
    assert_eq!(
        created.to_value(),
        json!({"project_id": 21, "repository_id": 34, "kind": "spec"})
    );
    assert_eq!(created.action(), "repository.created");
    assert_eq!(created.event_type(), Some(EventType::RepositoryCreated));
    assert_eq!(
        created.event_type().map(EventType::as_str),
        Some(created.action())
    );
    let requested = AuditDetails::RepositoryReprovisionRequested {
        repository_id: RepoId::new(34),
        job_id: 9,
        after_succeeded_job: None,
    };
    assert_eq!(
        requested.to_value(),
        json!({"repository_id": 34, "job_id": 9, "holder_job_id": null, "holder_state": "none"})
    );
    let after = AuditDetails::RepositoryReprovisionRequested {
        repository_id: RepoId::new(34),
        job_id: 10,
        after_succeeded_job: Some(9),
    };
    assert_eq!(
        after.to_value(),
        json!({"repository_id": 34, "job_id": 10, "holder_job_id": 9, "holder_state": "succeeded"})
    );
    assert_eq!(requested.action(), "repository.reprovision_requested");
    // An operator's recovery step: audit-only, no event.
    assert_eq!(requested.event_type(), None);
    for details in [created, requested] {
        let mut found = Vec::new();
        keys(&details.to_value(), &mut found);
        for pii in PII_KEYS {
            assert!(!found.iter().any(|k| k == pii), "{pii}");
        }
    }
}
// ── end D4 MR3b ──
