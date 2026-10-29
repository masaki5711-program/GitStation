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
    };
    // The exact shape (a Null or an empty object would pass a key scan).
    assert_eq!(
        created.to_value(),
        json!({"kind": "human", "namespace_id": 5, "is_admin": false})
    );
    assert_eq!(disabled.to_value(), json!({"kind": "service_account"}));
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
    for details in [created, disabled, group, added, changed, removed] {
        let mut found = Vec::new();
        keys(&details.to_value(), &mut found);
        for pii in PII_KEYS {
            assert!(!found.iter().any(|k| k == pii), "{pii} in {details:?}");
        }
    }
}
