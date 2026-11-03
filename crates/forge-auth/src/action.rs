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
    // --- REST API: merge requests (M3 R1a, ADR-0024 §12) ---
    /// `POST /api/v1/projects/{id}/merge-requests`: open a merge request
    /// (Developer and above, like `RepoPush`; opening one is not a push, so
    /// the source branch's protection is not consulted, plan 11 §11.7).
    MergeRequestCreate,
    /// `GET /api/v1/projects/{id}/merge-requests/{iid}`: as `RepoBrowse`.
    MergeRequestRead,
    /// `GET /api/v1/projects/{id}/merge-requests`: as `RepoBrowse`.
    MergeRequestList,
    /// `PATCH …/merge-requests/{iid}`: title, description, draft, target.
    /// The author or Developer and above; another's draft flag only the
    /// author or Maintainer and above.
    MergeRequestUpdate,
    /// `POST …/merge-requests/{iid}/close`: the author or Developer and above.
    MergeRequestClose,
    /// `POST …/merge-requests/{iid}/reopen`: as close; a merged one never.
    MergeRequestReopen,
    /// `POST …/merge-requests/{iid}/version-refs/purge` (M3 R1b, ADR-0024
    /// §2): remove the hidden version refs (secret removal). A human
    /// administrator's web session only: every token — an `admin`-scoped
    /// one included — is `FG-AUTH-0002 reason=session_required`; agents and
    /// service accounts hold only tokens (the sign-in admits people only,
    /// `not_human`), so they never reach it.
    MergeRequestVersionRefsPurge,
    /// `POST …/merge-requests/{iid}/discussions` and `…/discussions/{d}/notes`
    /// (M3 R3, ADR-0024 §12): start a discussion or reply to one. Reporter
    /// and above by membership; agents' tokens and service accounts too (a
    /// comment is no merge condition; resolving is `DiscussionResolve`).
    MergeRequestComment,
    /// `POST …/discussions/{d}/resolve` and `…/unresolve` (M3 R3,
    /// `Resource::Discussion`): people only (agents, service accounts and
    /// bots are refused); the thread's starter, or a Developer and above
    /// who is not the merge request's author (administrators included).
    DiscussionResolve,
    /// `POST …/merge-requests/{iid}/merge` (M3 R4, ADR-0024 §4.3, §12):
    /// merge a merge request. People only (agents, service accounts and
    /// bots are refused); the target branch's `push_min_role` and above
    /// (Developer when unprotected) — the direct-push ban of a protected
    /// branch (R6) does not stop it. A token needs `merge_execute` (`api`
    /// does not satisfy it, ADR-0017 §4.0). Whether the merge request is
    /// ready is `evaluate_merge()`'s, never a permission.
    MergeRequestMerge,
    /// `POST …/merge-requests/{iid}/approve` (M3 R5, ADR-0024 §8, §12):
    /// approve the head and target the caller saw. People only (agents,
    /// service accounts and bots are refused); Developer and above by
    /// membership; never the merge request's author, administrators
    /// included (`FG-REVIEW-0013`). A token needs `merge_approve` (`api`
    /// and `merge_execute` do not satisfy it). Whether the approval counts
    /// (rules, CODEOWNERS, committers) is `evaluate_merge()`'s, decided at
    /// evaluation time.
    MergeRequestApprove,
    /// `POST …/merge-requests/{iid}/unapprove` (M3 R5): withdraw one's own
    /// approval; decided like `MergeRequestApprove`.
    MergeRequestUnapprove,
    /// `PUT …/approval-rules/{name}`, `DELETE …/approval-rules/{name}` (M3
    /// R5, ADR-0024 §8.6, §12): manage a project's approval rules. People
    /// only (a rule is a merge condition); Maintainer and above; `api`.
    ApprovalRuleManage,
    /// `PUT /api/v1/projects/{id}/merge-settings` (M3 R6, ADR-0024 §4.1,
    /// §8.2, §8.3, §12): the methods a project allows and their default,
    /// the source deletion default, whether new commits clear approvals,
    /// whether committers' approvals are excluded. People only (they are
    /// merge conditions); Maintainer and above; `api`.
    MergeSettingsManage,
    /// `PUT /api/v1/projects/{id}/merge-settings/codeowners-enforcement`
    /// (M3 R6, ADR-0024 §9, `Resource::CodeownersEnforcement`): suspend
    /// CODEOWNERS enforcement for at most 24 hours (an expiry is required)
    /// or enforce it again. Its own action (not part of
    /// `MergeSettingsManage`): people only, Maintainer and above, and a
    /// suspension also needs a role at or above every protection rule of
    /// the project (`above_own_role`); `api`.
    CodeownersEnforcementToggle,
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
    // --- REST API: service accounts (M3 S0, ADR-0019 Addendum S0) ---
    /// `POST /api/v1/admin/service-accounts`: create a service account
    /// (an owner who is a person, a purpose). A human administrator's web
    /// session only: every token is `FG-AUTH-0002 reason=session_required`.
    ServiceAccountCreate,
    /// `GET /api/v1/admin/service-accounts`: administrators; a token needs
    /// the `admin` scope (and `read_api`).
    ServiceAccountList,
    /// `GET /api/v1/admin/service-accounts/{id}` (`Resource::ServiceAccount`):
    /// administrators; a user of another kind is not found
    /// (`reason=not_service_account`).
    ServiceAccountRead,
    /// `PATCH /api/v1/admin/service-accounts/{id}`: display name, purpose,
    /// owner. Administrators, people only (who answers for an account is a
    /// person's decision); `api` + `admin` on a token.
    ServiceAccountUpdate,
    /// `POST /api/v1/admin/service-accounts/{id}/tokens`
    /// (`Resource::ServiceAccountTokenRequest`): issue a token for an active
    /// service account. A human administrator's web session only (a token
    /// never issues a token); never with `admin`; a disabled account is
    /// refused (`target_disabled`).
    ServiceAccountTokenIssue,
    /// `GET /api/v1/admin/service-accounts/{id}/tokens`: administrators
    /// (`admin` + `read_api` on a token).
    ServiceAccountTokenList,
    /// `DELETE /api/v1/admin/service-accounts/{id}/tokens/{token_id}`:
    /// administrators, an `admin`-scoped token included (a leak is answered
    /// from wherever the administrator is, ADR-0019 Addendum S0).
    ServiceAccountTokenRevoke,
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
    /// `GET /api/v1/user/sessions`: one's own web sessions (M2 MR6,
    /// ADR-0021 §17, `Resource::Session`). Sessions only, so a token is
    /// refused before anything is looked up.
    SessionList,
    /// `DELETE /api/v1/user/sessions/{public_id}`: end one of one's own
    /// sessions; another user's is not found, administrators included.
    /// Sessions only.
    SessionRevoke,
    /// `GET /api/v1/admin/users/{id}/sessions`: a user's sessions
    /// (administrator rights; a token needs the `admin` scope).
    AdminSessionList,
    /// `DELETE /api/v1/admin/users/{id}/sessions`: end every session of a
    /// user (administrator rights, never an agent).
    AdminSessionRevoke,
    /// `GET /-/auth/login`, `GET /-/auth/callback`: start and finish the
    /// OIDC sign-in (M2 MR4, ADR-0021 §3). Public: the caller has no
    /// session yet; what the callback admits is decided by `SessionCreate`.
    WebLogin,
    /// Inside `GET /-/auth/callback`: may the person the IdP signed in get
    /// a session (`Resource::Admission`, ADR-0021 §3, §17)? Decided for the
    /// anonymous principal on the admission facts only — the IdP's access
    /// role and the account the identity belongs to (or `New`).
    SessionCreate,
    /// On a cookie-authenticated request whose 5-minute refresh check ran
    /// (M2 MR5, ADR-0021 §7, §17): may the session go on after the
    /// identity provider answered? Decided like `SessionCreate` on
    /// `Resource::Admission` alone, but only an active account with the
    /// access role continues (`New` is refused: a session's account exists).
    SessionContinue,
    /// `POST /-/auth/logout`: end the caller's own web session and send the
    /// browser to the identity provider's logout (M2 MR5, ADR-0021 §8).
    /// Sessions only (a token is `FG-AUTH-0002 reason=session_required`).
    SessionLogout,
    /// `POST /-/auth/backchannel-logout`: the identity provider ends
    /// sessions (OIDC Back-Channel Logout, M2 MR5, ADR-0021 §8). Public:
    /// the caller is the IdP's server, proven by the signed logout token,
    /// not by a credential (cookies and `Authorization` are never read).
    OidcBackchannelLogout,
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
        matches!(
            self,
            Action::Health | Action::WebShell | Action::WebLogin | Action::OidcBackchannelLogout
        )
    }

    /// Decided on `Resource::Admission` alone, whoever the principal is
    /// (the sign-in has no principal yet, ADR-0021 §3). With any other
    /// resource it is decided like every other action (an anonymous
    /// caller is `FG-AUTH-0001`).
    pub fn is_admission(self) -> bool {
        matches!(self, Action::SessionCreate | Action::SessionContinue)
    }

    /// Only a web session may perform it (ADR-0021 §17): a personal access
    /// token is refused whatever its scopes (`FG-AUTH-0002
    /// reason=session_required`, before the resource is looked at).
    pub fn is_session_only(self) -> bool {
        matches!(
            self,
            Action::SessionRead
                | Action::SessionLogout
                | Action::TokenCreate
                | Action::SessionList
                | Action::SessionRevoke
                | Action::MergeRequestVersionRefsPurge
                | Action::ServiceAccountCreate
                | Action::ServiceAccountTokenIssue
        )
    }

    /// Kept for people (ADR-0017 §4.0 amendments, ADR-0024 §8.4): an
    /// agent's token, a service account and a bot are refused whatever
    /// their role (`authorize()`'s `people_only`, `reason=agent_token` /
    /// `service_account` / `bot`). The matrix holds a service account and
    /// a bot deny row for each, allowed for the same person
    /// (`every_people_only_action_denies_service_accounts_and_bots`).
    pub fn is_people_only(self) -> bool {
        matches!(
            self,
            Action::ProjectVisibilityChange
                | Action::ProtectedRefManage
                | Action::ProjectTransfer
                | Action::GroupTransfer
                | Action::UserRename
                | Action::NamespaceTombstoneRelease
                | Action::ProjectDelete
                | Action::ProjectRestore
                | Action::MemberManage
                | Action::UserDisable
                | Action::AdminSessionRevoke
                | Action::MergeRequestVersionRefsPurge
                | Action::DiscussionResolve
                | Action::MergeRequestMerge
                | Action::MergeRequestApprove
                | Action::MergeRequestUnapprove
                | Action::ApprovalRuleManage
                | Action::MergeSettingsManage
                | Action::CodeownersEnforcementToggle
                | Action::ServiceAccountCreate
                | Action::ServiceAccountUpdate
                | Action::ServiceAccountTokenIssue
        )
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
            | Action::SessionList
            | Action::SessionRevoke
            | Action::WebLogin
            | Action::SessionCreate
            | Action::SessionContinue
            | Action::SessionLogout
            | Action::MergeRequestVersionRefsPurge
            | Action::ServiceAccountCreate
            | Action::ServiceAccountTokenIssue
            | Action::OidcBackchannelLogout => None,
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
            | Action::AdminSessionList
            | Action::UserSearch
            | Action::UserAdminList
            | Action::AuditRead
            | Action::UserSelfRead
            | Action::PathResolve
            | Action::MergeRequestRead
            | Action::MergeRequestList
            | Action::ServiceAccountList
            | Action::ServiceAccountRead
            | Action::ServiceAccountTokenList
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
            | Action::AdminSessionRevoke
            | Action::UserCreate
            | Action::UserDisable
            | Action::MergeRequestCreate
            | Action::MergeRequestUpdate
            | Action::MergeRequestClose
            | Action::MergeRequestReopen
            | Action::MergeRequestComment
            | Action::DiscussionResolve
            | Action::ApprovalRuleManage
            | Action::MergeSettingsManage
            | Action::CodeownersEnforcementToggle
            | Action::ServiceAccountUpdate
            | Action::ServiceAccountTokenRevoke => Some(Scope::Api),
            // M3 R4 (ADR-0024 §8.5): satisfied by `merge_execute` alone.
            Action::MergeRequestMerge => Some(Scope::MergeExecute),
            // M3 R5: satisfied by `merge_approve` alone.
            Action::MergeRequestApprove | Action::MergeRequestUnapprove => {
                Some(Scope::MergeApprove)
            }
        }
    }
}
