//! Typed `audit_log.details` (ADR-0016 §9). Variants carry no personal data
//! (no email, employee number, external id, display name, IP).

use crate::event_type::EventType;
use crate::ids::{NamespaceId, ProjectId, RepoId, UserId};
use crate::repository::RepositoryKind;
use crate::role::{Role, Visibility};
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
        /// The operator passed the administration guard (`--force`,
        /// ADR-0019 §10), whether or not it would have refused.
        forced: bool,
        /// Top-level groups this disable left without a direct active Owner
        /// (non-zero only when `forced`).
        ownerless_groups: i64,
        /// This disable left no active human administrator (only when `forced`).
        left_without_admin: bool,
    },
    /// `user.admin_bootstrapped` (D3 MR2, ADR-0019 Amendment): the recovery /
    /// first-install path made an active human an administrator. `created`:
    /// the user was created by the same command (then `namespace_id` is its
    /// personal namespace; no separate `user.created` row).
    AdminBootstrapped {
        created: bool,
        namespace_id: Option<NamespaceId>,
    },
    /// `group.created` (D2): its namespace, the parent group's namespace
    /// (`None` at the top level) and the first Owner.
    GroupCreated {
        namespace_id: NamespaceId,
        parent_namespace_id: Option<NamespaceId>,
        owner: UserId,
    },
    /// `project.created` (D4 MR2): the project, its namespace, the parent
    /// (a user or group namespace), its repository with the repository's
    /// kind, the visibility and the creator (the first direct Owner). Also
    /// the payload of the `project.created` event (integration class), so
    /// ids, kind and visibility only — never the path or the names.
    ProjectCreated {
        project_id: ProjectId,
        namespace_id: NamespaceId,
        parent_namespace_id: NamespaceId,
        repository_id: RepoId,
        kind: RepositoryKind,
        visibility: Visibility,
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
    // ── D4 MR3b: repository provisioning ──
    /// `repository.created` (D4 MR3b): the provisioning job created (or
    /// adopted) the bare repository and marked it ready. Also the payload of
    /// the `repository.created` event (integration class): ids and kind only.
    RepositoryCreated {
        project_id: ProjectId,
        repository_id: RepoId,
        kind: RepositoryKind,
    },
    /// `repository.reprovision_requested` (D4 MR3b, audit-only): an
    /// operator enqueued the provisioning job again (`forge admin repo
    /// reprovision`) for a repository still `provisioning`.
    RepositoryReprovisionRequested { repository_id: RepoId, job_id: i64 },
    // ── end D4 MR3b ──
}

impl AuditDetails {
    /// The `audit_log.action` this variant belongs to.
    pub fn action(&self) -> &'static str {
        match self {
            Self::UserCreated { .. } => "user.created",
            Self::UserDisabled { .. } => "user.disabled",
            Self::AdminBootstrapped { .. } => "user.admin_bootstrapped",
            Self::GroupCreated { .. } => "group.created",
            Self::ProjectCreated { .. } => "project.created",
            Self::MemberAdded { .. } => "member.added",
            Self::MemberRoleChanged { .. } => "member.role_changed",
            Self::MemberRemoved { .. } => "member.removed",
            Self::TokenCreated { .. } => "token.created",
            Self::TokenRevoked { .. } => "token.revoked",
            // ── D4 MR3b ──
            Self::RepositoryCreated { .. } => "repository.created",
            Self::RepositoryReprovisionRequested { .. } => "repository.reprovision_requested",
            // ── end D4 MR3b ──
        }
    }

    /// The event a change with these details publishes, `None` for the
    /// audit-only actions (`user.*`, `group.*`, `member.*` of groups; D-plan
    /// Q4, Q8).
    /// When `Some`, its name is [`Self::action`].
    pub fn event_type(&self) -> Option<EventType> {
        match self {
            Self::UserCreated { .. }
            | Self::UserDisabled { .. }
            | Self::AdminBootstrapped { .. }
            | Self::GroupCreated { .. }
            | Self::MemberAdded { .. }
            | Self::MemberRoleChanged { .. }
            | Self::MemberRemoved { .. } => None,
            Self::ProjectCreated { .. } => Some(EventType::ProjectCreated),
            Self::TokenCreated { .. } => Some(EventType::TokenCreated),
            Self::TokenRevoked { .. } => Some(EventType::TokenRevoked),
            // ── D4 MR3b ──
            Self::RepositoryCreated { .. } => Some(EventType::RepositoryCreated),
            Self::RepositoryReprovisionRequested { .. } => None,
            // ── end D4 MR3b ──
        }
    }

    /// The JSON stored in `audit_log.details`:
    /// UserCreated `{"kind","namespace_id","is_admin"}`,
    /// UserDisabled `{"kind","revoked_tokens","forced","ownerless_groups","left_without_admin"}`,
    /// AdminBootstrapped `{"created","namespace_id"}`,
    /// GroupCreated `{"namespace_id","parent_namespace_id","owner_id"}`,
    /// ProjectCreated `{"project_id","namespace_id","parent_namespace_id",
    /// "repository_id","kind","visibility","owner_id"}`,
    /// MemberAdded / MemberRemoved `{"user_id","role"}`,
    /// MemberRoleChanged `{"user_id","from","to"}`,
    /// TokenCreated `{"token_id","user_id","public_id","scopes","expires_at","is_agent"}`,
    /// TokenRevoked `{"token_id","user_id","public_id"}`,
    /// RepositoryCreated `{"project_id","repository_id","kind"}`,
    /// RepositoryReprovisionRequested `{"repository_id","job_id"}`.
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
                forced,
                ownerless_groups,
                left_without_admin,
            } => serde_json::json!({
                "kind": kind.as_str(),
                "revoked_tokens": revoked_tokens,
                "forced": forced,
                "ownerless_groups": ownerless_groups,
                "left_without_admin": left_without_admin,
            }),
            Self::AdminBootstrapped {
                created,
                namespace_id,
            } => serde_json::json!({
                "created": created,
                "namespace_id": namespace_id.map(NamespaceId::get),
            }),
            Self::GroupCreated {
                namespace_id,
                parent_namespace_id,
                owner,
            } => serde_json::json!({
                "namespace_id": namespace_id.get(),
                "parent_namespace_id": parent_namespace_id.map(NamespaceId::get),
                "owner_id": owner.get(),
            }),
            Self::ProjectCreated {
                project_id,
                namespace_id,
                parent_namespace_id,
                repository_id,
                kind,
                visibility,
                owner,
            } => serde_json::json!({
                "project_id": project_id.get(),
                "namespace_id": namespace_id.get(),
                "parent_namespace_id": parent_namespace_id.get(),
                "repository_id": repository_id.get(),
                "kind": kind.as_str(),
                "visibility": visibility.as_str(),
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
            // ── D4 MR3b ──
            Self::RepositoryCreated {
                project_id,
                repository_id,
                kind,
            } => serde_json::json!({
                "project_id": project_id.get(),
                "repository_id": repository_id.get(),
                "kind": kind.as_str(),
            }),
            Self::RepositoryReprovisionRequested {
                repository_id,
                job_id,
            } => serde_json::json!({
                "repository_id": repository_id.get(),
                "job_id": job_id,
            }),
            // ── end D4 MR3b ──
        }
    }
}
