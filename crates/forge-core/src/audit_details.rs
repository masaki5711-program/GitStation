//! Typed `audit_log.details` (ADR-0016 §9). Variants carry no personal data
//! (no email, employee number, external id, display name, IP).

use crate::ids::NamespaceId;
use crate::user_kind::UserKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditDetails {
    /// `user.created`.
    UserCreated {
        kind: UserKind,
        namespace_id: NamespaceId,
        is_admin: bool,
    },
    /// `user.disabled`.
    UserDisabled { kind: UserKind },
}

impl AuditDetails {
    /// The `audit_log.action` this variant belongs to.
    pub fn action(&self) -> &'static str {
        match self {
            Self::UserCreated { .. } => "user.created",
            Self::UserDisabled { .. } => "user.disabled",
        }
    }

    /// The JSON stored in `audit_log.details`:
    /// UserCreated `{"kind","namespace_id","is_admin"}`, UserDisabled `{"kind"}`.
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::UserCreated {
                kind,
                namespace_id,
                is_admin,
            } => serde_json::json!({
                "kind": kind.as_str(),
                "namespace_id": namespace_id.get(),
                "is_admin": is_admin,
            }),
            Self::UserDisabled { kind } => serde_json::json!({ "kind": kind.as_str() }),
        }
    }
}
