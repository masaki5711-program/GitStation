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
    /// The repository browse API (`GET /api/v1/projects/{id}/repository/…`:
    /// refs, resolve, tree, blob; M2 MR7, ADR-0015 Amendment M2). Decided
    /// like `RepoRead` (the same arm), but with the `read_api` scope: a
    /// `read_repository` token (CI clones) does not read the JSON API, and a
    /// `read_api` token still cannot clone.
    RepoBrowse,
    // --- REST API: projects ---
    ProjectRead,
    /// `GET /api/v1/projects`: may the caller list at all (any active
    /// signed-in user; `read_api` on a token). Decided on the instance;
    /// every row is then decided again with `ProjectRead` (M1 phase 8b).
    ProjectList,
    ProjectCreate,
    ProjectUpdate,
    /// Private ⇄ internal: Owner only (exposes a project to every signed-in user).
    ProjectVisibilityChange,
    /// Move or rename a project (`Resource::Transfer`, ADR-0015).
    ProjectTransfer,
    ProjectDelete,
    ProjectRestore,
    ProtectedRefManage,
    MemberManage,
    /// `GET /api/v1/projects/{id}/members`, `GET /api/v1/groups/{id}/members`:
    /// the direct members (M1 phase 8b MR8). A project's as `ProjectRead`
    /// (Reporter; every signed-in user on an internal project), a group's
    /// for every signed-in user (groups are internal).
    MemberList,
    /// `GET /api/v1/projects/{id}/protected-refs`: the protection rules
    /// (M1 phase 8b MR8), as `ProjectRead`.
    ProtectedRefList,
    // --- REST API: groups ---
    GroupRead,
    /// `GET /api/v1/groups`: as `ProjectList`; every row is then decided
    /// with `GroupRead` (M1 phase 8b).
    GroupList,
    GroupCreateTopLevel,
    SubgroupCreate,
    GroupUpdate,
    /// Move or rename a group (`Resource::GroupTransfer`, ADR-0015, D5).
    GroupTransfer,
    /// Release a namespace tombstone (`Resource::Tombstone`, ADR-0015, D5).
    NamespaceTombstoneRelease,
    // --- REST API: tokens and users ---
    /// `POST /api/v1/user/tokens` (M2 MR6): create a token for oneself.
    /// Sessions only (ADR-0019 Amendment M2 4): a token never creates a
    /// token (`FG-AUTH-0002 reason=session_required`).
    TokenCreate,
    /// `GET /api/v1/user/tokens` (M2 MR6): one's own tokens.
    TokenList,
    /// `DELETE /api/v1/user/tokens/{id}` (M2 MR6).
    TokenRevoke,
    UserCreate,
    /// `POST /api/v1/admin/users/{id}/disable` (M2 MR6): administrators,
    /// never an agent; the administration guard always holds over HTTP
    /// (`users::Guard::Enforce`, ADR-0019 §10).
    UserDisable,
    /// `GET /api/v1/users?search=` (M2 MR6): find users by name for the
    /// member picker (any active signed-in user; names only).
    UserSearch,
    /// `GET /api/v1/admin/users` (M2 MR6): every user with its state
    /// (administrators; a token needs the `admin` scope).
    UserAdminList,
    /// Change a user's username and personal namespace path
    /// (`Resource::UserRename`, ADR-0015, D5): administrators only.
    UserRename,
    AuditRead,
    /// `GET /api/v1/user`: the caller's own account (any active signed-in
    /// user; `read_api` on a token). Decided on the instance: there is no
    /// other user to see.
    UserSelfRead,
    /// `GET /api/v1/session`: the caller's own web session — who it stands
    /// for and its CSRF token (M2 MR3, ADR-0021 §6, §17). Sessions only: a
    /// token is `FG-AUTH-0002 reason=session_required` (a token has no
    /// session to read, and must never learn a CSRF token).
    SessionRead,
    /// `GET /api/v1/user/sessions` (one's own, sessions only) and `GET
    /// /api/v1/admin/users/{id}/sessions` (administrators), M2 MR6,
    /// ADR-0021 §17 (`Resource::Session`).
    SessionList,
    /// `DELETE /api/v1/user/sessions/{id}` (one's own, sessions only) and
    /// `DELETE /api/v1/admin/users/{id}/sessions` (administrators, every
    /// active session of the user), M2 MR6, ADR-0021 §17.
    SessionRevoke,
    /// `GET /-/auth/login`, `GET /-/auth/callback`: start and finish the
    /// OIDC sign-in (M2 MR4, ADR-0021 §3). Public: the caller has no
    /// session yet; what the callback admits is decided by `SessionCreate`.
    WebLogin,
    /// Inside `GET /-/auth/callback`: may the person the IdP signed in get
    /// a session (`Resource::Admission`, ADR-0021 §3, §17)? Decided for the
    /// anonymous principal on the admission facts only — the IdP's access
    /// role and the account the identity belongs to (or `New`).
    SessionCreate,
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
        matches!(self, Action::Health | Action::WebShell | Action::WebLogin)
    }

    /// Decided on `Resource::Admission` alone, whoever the principal is
    /// (the sign-in has no principal yet, ADR-0021 §3). With any other
    /// resource it is decided like every other action (an anonymous
    /// caller is `FG-AUTH-0001`).
    pub fn is_admission(self) -> bool {
        matches!(self, Action::SessionCreate)
    }

    /// Only a web session may perform it (ADR-0021 §17): a personal access
    /// token is refused whatever its scopes (`FG-AUTH-0002
    /// reason=session_required`, before the resource is looked at).
    pub fn is_session_only(self) -> bool {
        matches!(self, Action::SessionRead | Action::TokenCreate)
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
            // Public, or session only (`is_session_only`: no token reaches
            // the scope check).
            // The sign-in: public, or decided on the admission facts.
            Action::Health
            | Action::WebShell
            | Action::SessionRead
            | Action::TokenCreate
            | Action::WebLogin
            | Action::SessionCreate => None,
            Action::RepoRead => Some(Scope::ReadRepository),
            Action::RepoPush => Some(Scope::WriteRepository),
            Action::ProjectRead
            | Action::RepoBrowse
            | Action::ProjectList
            | Action::MemberList
            | Action::ProtectedRefList
            | Action::GroupRead
            | Action::GroupList
            | Action::TokenList
            | Action::SessionList
            | Action::UserSearch
            | Action::UserAdminList
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
            | Action::GroupTransfer
            | Action::NamespaceTombstoneRelease
            | Action::UserRename
            | Action::TokenRevoke
            | Action::SessionRevoke
            | Action::UserCreate
            | Action::UserDisable => Some(Scope::Api),
        }
    }
}
