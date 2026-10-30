//! Typed `audit_log.details` (ADR-0016 §9). Variants carry no personal data
//! (no email, employee number, external id, display name, IP).

use crate::ids::{NamespaceId, UserId};
use crate::role::Role;
use crate::scope::Scope;
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
    UserDisabled {
        kind: UserKind,
        /// Active tokens revoked in the same change (D3, ADR-0019).
        revoked_tokens: i64,
    },
    /// `group.created` (D2): its namespace, the parent group's namespace
    /// (`None` at the top level) and the first Owner.
    GroupCreated {
        namespace_id: NamespaceId,
        parent_namespace_id: Option<NamespaceId>,
        owner: UserId,
    },
    /// `member.added` (D2, group membership).
    MemberAdded { user: UserId, role: Role },
    /// `member.role_changed` (D2).
    MemberRoleChanged { user: UserId, from: Role, to: Role },
    /// `member.removed` (D2): the role the member had.
    MemberRemoved { user: UserId, role: Role },
    /// `token.created` (D3): ids, the public id, scope names,
    /// the expiry (RFC 3339, UTC) and `is_agent`. Never the secret, its hash or the name.
    TokenCreated {
        token_id: i64,
        user: UserId,
        public_id: String,
        scopes: Vec<Scope>,
        expires_at: String,
        is_agent: bool,
    },
    /// `token.revoked` (D3).
    TokenRevoked {
        token_id: i64,
        user: UserId,
        public_id: String,
    },
}

impl AuditDetails {
    /// The `audit_log.action` this variant belongs to.
    pub fn action(&self) -> &'static str {
        match self {
            Self::UserCreated { .. } => "user.created",
            Self::UserDisabled { .. } => "user.disabled",
            Self::GroupCreated { .. } => "group.created",
            Self::MemberAdded { .. } => "member.added",
            Self::MemberRoleChanged { .. } => "member.role_changed",
            Self::MemberRemoved { .. } => "member.removed",
            Self::TokenCreated { .. } => "token.created",
            Self::TokenRevoked { .. } => "token.revoked",
        }
    }

    /// The JSON stored in `audit_log.details`:
    /// UserCreated `{"kind","namespace_id","is_admin"}`, UserDisabled `{"kind","revoked_tokens"}`,
    /// GroupCreated `{"namespace_id","parent_namespace_id","owner_id"}`,
    /// MemberAdded / MemberRemoved `{"user_id","role"}`,
    /// MemberRoleChanged `{"user_id","from","to"}`,
    /// TokenCreated `{"token_id","user_id","public_id","scopes","expires_at","is_agent"}`,
    /// TokenRevoked `{"token_id","user_id","public_id"}`.
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
            Self::UserDisabled {
                kind,
                revoked_tokens,
            } => serde_json::json!({ "kind": kind.as_str(), "revoked_tokens": revoked_tokens }),
            Self::GroupCreated {
                namespace_id,
                parent_namespace_id,
                owner,
            } => serde_json::json!({
                "namespace_id": namespace_id.get(),
                "parent_namespace_id": parent_namespace_id.map(NamespaceId::get),
                "owner_id": owner.get(),
            }),
            Self::MemberAdded { user, role } | Self::MemberRemoved { user, role } => {
                serde_json::json!({ "user_id": user.get(), "role": role.as_str() })
            }
            Self::MemberRoleChanged { user, from, to } => serde_json::json!({
                "user_id": user.get(),
                "from": from.as_str(),
                "to": to.as_str(),
            }),
            Self::TokenCreated {
                token_id,
                user,
                public_id,
                scopes,
                expires_at,
                is_agent,
            } => serde_json::json!({
                "token_id": token_id,
                "user_id": user.get(),
                "public_id": public_id,
                "scopes": scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                "expires_at": expires_at,
                "is_agent": is_agent,
            }),
            Self::TokenRevoked {
                token_id,
                user,
                public_id,
            } => serde_json::json!({
                "token_id": token_id,
                "user_id": user.get(),
                "public_id": public_id,
            }),
        }
    }
}
