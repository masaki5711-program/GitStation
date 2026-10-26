//! Every operation that needs a decision. Adding a variant breaks the build
//! of `authorize()` (exhaustive match, no `_` arm) and fails the matrix tests
//! until its rows exist (CLAUDE.md §3-4, ADR-0010).

use crate::model::Scope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    // --- git operations ---
    /// git clone / fetch / ls-remote (upload-pack).
    RepoRead,
    /// One ref update in a git push (receive-pack).
    RepoPush,
    // --- REST API: projects ---
    ProjectRead,
    ProjectCreate,
    ProjectUpdate,
    ProjectTransfer,
    ProjectDelete,
    ProjectRestore,
    ProtectedRefManage,
    MemberManage,
    // --- REST API: groups ---
    GroupRead,
    GroupCreateTopLevel,
    SubgroupCreate,
    GroupUpdate,
    // --- REST API: tokens and users ---
    TokenCreate,
    TokenList,
    TokenRevoke,
    UserCreate,
    UserDisable,
    AuditRead,
    // --- operations ---
    Health,
    Metrics,
}

impl Action {
    /// Every action, for the matrix tests and the coverage manifest.
    pub const ALL: [Action; 22] = [
        Action::RepoRead,
        Action::RepoPush,
        Action::ProjectRead,
        Action::ProjectCreate,
        Action::ProjectUpdate,
        Action::ProjectTransfer,
        Action::ProjectDelete,
        Action::ProjectRestore,
        Action::ProtectedRefManage,
        Action::MemberManage,
        Action::GroupRead,
        Action::GroupCreateTopLevel,
        Action::SubgroupCreate,
        Action::GroupUpdate,
        Action::TokenCreate,
        Action::TokenList,
        Action::TokenRevoke,
        Action::UserCreate,
        Action::UserDisable,
        Action::AuditRead,
        Action::Health,
        Action::Metrics,
    ];

    /// Stable name used in logs, the route registry and the manifests.
    pub fn name(self) -> &'static str {
        match self {
            Action::RepoRead => "RepoRead",
            Action::RepoPush => "RepoPush",
            Action::ProjectRead => "ProjectRead",
            Action::ProjectCreate => "ProjectCreate",
            Action::ProjectUpdate => "ProjectUpdate",
            Action::ProjectTransfer => "ProjectTransfer",
            Action::ProjectDelete => "ProjectDelete",
            Action::ProjectRestore => "ProjectRestore",
            Action::ProtectedRefManage => "ProtectedRefManage",
            Action::MemberManage => "MemberManage",
            Action::GroupRead => "GroupRead",
            Action::GroupCreateTopLevel => "GroupCreateTopLevel",
            Action::SubgroupCreate => "SubgroupCreate",
            Action::GroupUpdate => "GroupUpdate",
            Action::TokenCreate => "TokenCreate",
            Action::TokenList => "TokenList",
            Action::TokenRevoke => "TokenRevoke",
            Action::UserCreate => "UserCreate",
            Action::UserDisable => "UserDisable",
            Action::AuditRead => "AuditRead",
            Action::Health => "Health",
            Action::Metrics => "Metrics",
        }
    }

    /// Reachable without credentials.
    pub fn is_public(self) -> bool {
        matches!(self, Action::Health)
    }

    /// Performed through the git protocol rather than an HTTP API route.
    pub fn is_git_operation(self) -> bool {
        matches!(self, Action::RepoRead | Action::RepoPush)
    }

    /// The token scope the action needs (`None` for public actions).
    pub fn required_scope(self) -> Option<Scope> {
        match self {
            Action::Health => None,
            Action::RepoRead => Some(Scope::ReadRepository),
            Action::RepoPush => Some(Scope::WriteRepository),
            Action::ProjectRead
            | Action::GroupRead
            | Action::TokenList
            | Action::AuditRead
            | Action::Metrics => Some(Scope::ReadApi),
            Action::ProjectCreate
            | Action::ProjectUpdate
            | Action::ProjectTransfer
            | Action::ProjectDelete
            | Action::ProjectRestore
            | Action::ProtectedRefManage
            | Action::MemberManage
            | Action::GroupCreateTopLevel
            | Action::SubgroupCreate
            | Action::GroupUpdate
            | Action::TokenCreate
            | Action::TokenRevoke
            | Action::UserCreate
            | Action::UserDisable => Some(Scope::Api),
        }
    }
}
