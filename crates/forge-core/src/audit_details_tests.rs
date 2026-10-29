use serde_json::{json, Value};

use crate::ids::NamespaceId;
use crate::{AuditDetails, UserKind};

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
    for details in [created, disabled] {
        let mut found = Vec::new();
        keys(&details.to_value(), &mut found);
        for pii in PII_KEYS {
            assert!(!found.iter().any(|k| k == pii), "{pii} in {details:?}");
        }
    }
}
