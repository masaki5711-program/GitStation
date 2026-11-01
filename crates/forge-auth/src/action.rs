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
    /// Private ⇄ internal: Owner only (exposes a project to every signed-in user).
    ProjectVisibilityChange,
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
    /// `GET /api/v1/user`: the caller's own account (any active signed-in
    /// user; `read_api` on a token). Decided on the instance: there is no
    /// other user to see.
    UserSelfRead,
    // --- web (ADR-0015) ---
    /// `GET /api/v1/namespaces/resolve?path=`: which namespace a path names,
    /// decided by whether the caller can see it (`Resource::Unresolved` /
    /// `Resource::Resolved`). Git paths are decided by `RepoRead` / `RepoPush`.
    PathResolve,
    /// The SPA shell (`index.html`) for namespace pages and permalinks: the
    /// same body for every path, without looking at the database (public).
    WebShell,
    // --- operations ---
    Health,
    Metrics,
}

impl Action {
    /// Reachable without credentials.
    pub fn is_public(self) -> bool {
        matches!(self, Action::Health | Action::WebShell)
    }

    /// Also performed through the git protocol (Smart HTTP). `RepoRead` is
    /// an HTTP route too (`/-/raw`, `/-/archive`, ADR-0015): `true` means
    /// "git does it as well", not "only git" (M1 phase 8 MR4).
    pub fn is_git_operation(self) -> bool {
        matches!(self, Action::RepoRead | Action::RepoPush)
    }

    /// The token scope the action needs (`None` for public actions).
    pub fn required_scope(self) -> Option<Scope> {
        match self {
            Action::Health | Action::WebShell => None,
            Action::RepoRead => Some(Scope::ReadRepository),
            Action::RepoPush => Some(Scope::WriteRepository),
            Action::ProjectRead
            | Action::GroupRead
            | Action::TokenList
            | Action::AuditRead
            | Action::UserSelfRead
            | Action::PathResolve
            | Action::Metrics => Some(Scope::ReadApi),
            Action::ProjectCreate
            | Action::ProjectUpdate
            | Action::ProjectVisibilityChange
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
