//! Every operation that needs a decision. The enum, `Action::ALL` and the
//! names are generated together by one macro, so a variant can never be
//! missing from `ALL`. A new variant breaks the build of `authorize()`
//! (exhaustive match) and fails the matrix tests until its rows exist
//! (CLAUDE.md §3-4, ADR-0010).

use crate::model::Scope;

macro_rules! actions {
    ($( $(#[$doc:meta])* $name:ident ),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Action {
            $( $(#[$doc])* $name, )+
        }

        impl Action {
            /// Every action, for the matrix tests and the coverage manifest.
            pub const ALL: &'static [Action] = &[ $( Action::$name ),+ ];

            /// Stable name used in logs, the route registry and the manifests.
            pub fn name(self) -> &'static str {
                match self {
                    $( Action::$name => stringify!($name), )+
                }
            }
        }
    };
}

actions! {
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
