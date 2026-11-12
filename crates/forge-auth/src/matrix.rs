//! The permission matrix: the specification of `authorize()` as data.
//!
//! Every action has at least one allowed and one denied row (public actions:
//! an allowed anonymous row), and every row is asserted by the tests. Only
//! the summary is public: it feeds `Git/docs/authz/matrix.generated.json`,
//! which the `authz-coverage` gate checks against the route registry.

use forge_errors::ErrorCode;

use crate::action::Action;
use crate::model::{AdmissionAccount, MembershipChange, RefKind, RefUpdate, Role};
// Turning rows into principals and resources is only needed by the tests.
#[cfg(test)]
use crate::model::{
    AccountKind, Credential, MembershipScope, NamespaceView, Principal, ProjectView, RefProtection,
    ResolvedVia, Resource, Scope, TombstoneRelease, TransferTarget, UnresolvedReason, Visibility,
};
#[cfg(test)]
use forge_core::UserId;

/// The caller in a row. Token actors hold only the listed scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Actor {
    Anonymous,
    Disabled,
    DisabledAdmin,
    NonMember,
    Guest,
    Reporter,
    Developer,
    Maintainer,
    Owner,
    Admin,
    /// Developer with a `read_repository` token.
    DevTokenReadRepository,
    /// Developer with a `write_repository` token.
    DevTokenWriteRepository,
    /// Developer with a `write_repository` token issued with `is_agent`
    /// (ADR-0017 §4.0). A push is decided exactly like the human token's
    /// (rows pin it); agents are denied only the actions kept for people.
    AgentTokenWriteRepository,
    /// Developer with a `read_api` token issued with `is_agent`.
    AgentTokenReadApi,
    /// Developer with an `api` token issued with `is_agent` (M1 phase 8b
    /// MR7a).
    AgentTokenApi,
    /// Owner with an `api` token issued with `is_agent`.
    AgentOwnerTokenApi,
    /// Administrator with an `api` + `admin` token issued with `is_agent`.
    AgentAdminTokenAdmin,
    /// Maintainer with an `api` token issued with `is_agent` (M1 phase 8b
    /// MR8: the role that would otherwise manage members and protections).
    AgentMaintainerTokenApi,
    /// Maintainer with an `api` token (M1 phase 8b MR8).
    MaintainerTokenApi,
    /// Developer with an `api` token.
    DevTokenApi,
    /// Owner with a `read_api` token.
    OwnerTokenReadApi,
    /// A user without a role, with a `read_api` token (M2 MR7).
    NonMemberTokenReadApi,
    /// Guest with a `read_api` token (M2 MR7).
    GuestTokenReadApi,
    /// Administrator with an `api` token (no `admin` scope).
    AdminTokenApi,
    /// Administrator with an `api` + `admin` token.
    AdminTokenAdmin,
    /// Developer with an `api` token on a service account (M3 R3,
    /// ADR-0024 §8.4: `users.kind = service_account`).
    ServiceAccountTokenApi,
    /// Developer with an `api` token on a bot account (`users.kind = bot`).
    BotTokenApi,
    /// Owner with an `api` token (a person; the twin of the service
    /// account / bot Owner actors, M3 R3 review).
    OwnerTokenApi,
    /// The service account / bot twins of `OwnerTokenApi`,
    /// `MaintainerTokenApi`, `AdminTokenAdmin` and `Admin` (a session: the
    /// sign-in never gives them one, but `authorize()` must refuse them on
    /// its own — ADR-0024 §8.4, M3 R3 review).
    ServiceAccountOwnerTokenApi,
    ServiceAccountMaintainerTokenApi,
    ServiceAccountAdminTokenAdmin,
    ServiceAccountAdminSession,
    BotOwnerTokenApi,
    BotMaintainerTokenApi,
    BotAdminTokenAdmin,
    BotAdminSession,
    /// Developer with a `merge_execute` token (M3 R4, ADR-0024 §8.5).
    DevTokenMergeExecute,
    /// Developer with a `merge_execute` token issued with `is_agent`.
    AgentTokenMergeExecute,
    /// The service account / bot twins of `DevTokenMergeExecute`.
    ServiceAccountTokenMergeExecute,
    BotTokenMergeExecute,
    /// Administrator with a `merge_execute` + `admin` token (review round 1:
    /// administrator rights apply on the merge as on a push).
    AdminTokenMergeExecuteAdmin,
    /// Administrator with a `merge_execute` token, no `admin` scope (acts as
    /// a normal user: no role on the fixture's project).
    AdminTokenMergeExecute,
    /// Developer with a `merge_approve` token (M3 R5, ADR-0024 §8.5).
    DevTokenMergeApprove,
    /// Developer with a `merge_approve` token issued with `is_agent`.
    AgentTokenMergeApprove,
    /// The service account / bot twins of `DevTokenMergeApprove`.
    ServiceAccountTokenMergeApprove,
    BotTokenMergeApprove,
    /// Maintainer with a `read_api` token (no `api`; R5 review).
    MaintainerTokenReadApi,
    /// Administrator with a `merge_approve` + `admin` token (R5 review).
    AdminTokenMergeApproveAdmin,
    /// Administrator with a `read_api` + `admin` token (M3 O2: reads the
    /// push mirror, does not change it).
    AdminTokenReadApiAdmin,
    /// A service account without a role, with a `read_api` token (M3 S0:
    /// it sees member projects only); the twin of `NonMemberTokenReadApi`.
    ServiceAccountNonMemberTokenReadApi,
    /// Developer with a `user_email` token only (M3 S0: it reads no API).
    DevTokenUserEmail,
    /// A service account that is a Guest member, with a `read_api` +
    /// `read_repository` token (S0 review: it reads by its Guest role).
    ServiceAccountGuestTokenRead,
    /// A bot without a role, with a `read_api` token (S0 review: the
    /// member-only rule is the service account's; a bot still sees
    /// internal projects).
    BotNonMemberTokenReadApi,
    /// A service account that is a Developer member, with a `read_api`
    /// token (M3 C1: it reads check runs, never reports them).
    ServiceAccountTokenReadApi,
    /// A service account that is a Reporter member, with an `api` token
    /// (M3 C1: below the reporters' Developer role).
    ServiceAccountReporterTokenApi,
    /// A service account that is a Developer member, with a `read_api`
    /// token issued with `is_agent` (M3 I1c: the feed refuses agents).
    ServiceAccountAgentTokenReadApi,
    /// A disabled service account that is a Developer member, with a
    /// `read_api` token (M3 I1c).
    ServiceAccountDisabledTokenReadApi,
    /// A Developer with a `read_repository` token issued with `is_agent`,
    /// and the service account / bot twins of `DevTokenReadRepository`
    /// (plan 06 D-5 safety net: whole copies, `matrix_rows_repo_copy.rs`).
    AgentTokenReadRepository,
    ServiceAccountTokenReadRepository,
    BotTokenReadRepository,
    /// An integration's service account with an `act_as` token (M3 S2:
    /// the claim), and its agent twin.
    ServiceAccountTokenActAs,
    ServiceAccountAgentTokenActAs,
    /// A service account acting for the person `ME` (M3 S2, ADR-0025):
    /// `matrix_rows_act_as.rs`.
    ActAs(crate::matrix_rows_act_as::Via),
    /// A registered CI runner (M4 C0, ADR-0029): `matrix_rows_ci.rs`.
    Runner,
    /// A CI job of `matrix_rows_ci::JOB_PROJECT` pinned at
    /// `matrix_rows_ci::JOB_SHA`, its token in this state.
    Job(crate::model::JobTokenState),
}

/// The resource in a row; the caller's role on it comes from the actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fixture {
    PrivateProject,
    InternalProject,
    DeletedProject,
    /// An internal project pending deletion (every signed-in user sees it).
    InternalDeletedProject,
    /// A whole copy (clone / fetch, an archive) of a private project's
    /// repository whose default tip was measured as the mark says (plan 06
    /// D-5 safety net).
    RepoCopy(crate::model::CopyMark),
    /// The same of an internal project.
    InternalRepoCopy(crate::model::CopyMark),
    /// A CI job's pinned copy (M4 C0, ADR-0029 §3) of the job's project
    /// (or another) at the job's SHA (or another).
    PinnedCopy {
        other_project: bool,
        other_sha: bool,
    },
    /// A CI job as the runner API sees it (M4 C3): leased to the row's
    /// runner (or to another runner, or to none) and running (or ended).
    RunnerJob {
        holder: crate::matrix_rows_ci::Holder,
        finished: bool,
    },
    /// A ref update on a private project; `protected` = minimum push role.
    Ref {
        kind: RefKind,
        update: RefUpdate,
        protected: Option<Role>,
    },
    /// A branch update on a private project whose Maintainer-level
    /// protection forbids direct pushes (M3 R6).
    RefNoDirectPush {
        update: RefUpdate,
    },
    /// A CODEOWNERS enforcement change (M3 R6) on a private project
    /// (`deleted`: pending deletion) whose highest protection is `highest`.
    Codeowners {
        highest: Option<Role>,
        change: crate::model::CodeownersToggle,
        deleted: bool,
    },
    /// A merge settings change (M3 R6 review) on a project of `project`'s
    /// kind whose highest protection is `highest`.
    SettingsChange {
        highest: Option<Role>,
        loosens: bool,
        project: crate::matrix_review::MrProject,
    },
    /// A branch fast-forward on an internal project.
    InternalRef,
    /// A branch fast-forward on a project pending deletion.
    DeletedRef,
    Group,
    OwnNamespace,
    ForeignNamespace,
    /// Another user's personal namespace whose owner is a service account
    /// (S0 review: nobody creates or moves a project into it).
    ServiceAccountNamespace,
    /// Moving a private project into a service account's namespace.
    TransferToServiceAccountNamespace,
    /// Moving a private project the caller has the actor's role on to a
    /// visible group where the caller has `target`.
    Transfer {
        target: Option<Role>,
    },
    /// Moving a private project to a group the caller cannot see (or that
    /// does not exist) (D5).
    TransferHiddenTarget,
    /// Moving a private project into the caller's own personal namespace.
    TransferToOwnNamespace,
    /// Moving a private project into another user's personal namespace.
    TransferToForeignNamespace,
    /// Moving a private project to the top level (never allowed).
    TransferToTopLevel,
    /// Moving a private project pending deletion to a group the caller
    /// owns.
    TransferDeletedSource,
    /// Moving a group the caller has the actor's role on to a visible group
    /// where the caller has `target` (D5).
    GroupTransfer {
        target: Option<Role>,
    },
    /// Moving a group to a group the caller cannot see.
    GroupTransferHiddenTarget,
    /// Moving a group to the top level.
    GroupTransferToTopLevel,
    /// Moving a group into the caller's own personal namespace (never).
    GroupTransferToNamespace,
    /// Renaming a private project the caller has the actor's role on: the
    /// target is its current parent group (D5 MR5).
    TransferRename,
    /// Renaming a group (below its current parent group).
    GroupTransferRename,
    /// Renaming a top-level group (it stays at the top level).
    GroupTransferRenameTopLevel,
    /// Renaming another user (D5).
    UserRenameOther,
    /// Renaming oneself.
    UserRenameSelf,
    /// Releasing a `moved` tombstone; the caller's role on its target is
    /// the actor's; `visible` = the caller can see the target (D5).
    ReleaseMoved {
        visible: bool,
    },
    /// Releasing a `deleted` tombstone with a parent; the caller's role on
    /// the parent is the actor's; `recorded` = the caller is the recorded
    /// owner (`owner_at_write`).
    ReleaseDeleted {
        recorded: bool,
    },
    /// Releasing a `deleted` tombstone whose recorded owner is the caller,
    /// while the caller has no role in its parent (whatever the actor).
    ReleaseDeletedNoRole,
    /// Releasing a `deleted` tombstone without a parent (a top-level group
    /// or a user) whose recorded owner is the caller.
    ReleaseTopLevel,
    ProjectMember(MembershipChange),
    InternalProjectMember(MembershipChange),
    DeletedProjectMember(MembershipChange),
    GroupMember(MembershipChange),
    /// Removing the last Owner of a private project the caller owns.
    LastOwnerRemoval,
    /// Removing the last Owner of a group.
    GroupLastOwnerRemoval,
    /// Demoting the last Owner of a private project to Maintainer (MR8).
    LastOwnerDemotion,
    /// Demoting the last Owner of a group to Maintainer (MR8).
    GroupLastOwnerDemotion,
    /// Changing (`Some`) or removing (`None`) a user who is no direct
    /// member of a private project (MR8).
    MissingMember(Option<Role>),
    /// The same on a group (MR8 review).
    GroupMissingMember(Option<Role>),
    /// Removing (`None`) or demoting (`Some(to)`) the last **direct**
    /// Owner of a subgroup: not the last Owner — every subgroup inherits
    /// the top-level group's Owner, which always keeps one (D2 / D4 MR4
    /// rule), so `removes_last_owner` is `false` (MR8 review).
    SubgroupLastDirectOwner(Option<Role>),
    /// Changing a protection's minimum push role (None = unprotected).
    ProtectionChange {
        from: Option<Role>,
        to: Option<Role>,
    },
    /// Adding a Maintainer protection on an internal project (MR8).
    InternalProtectionChange,
    /// Adding a Maintainer protection on a project pending deletion (MR8).
    DeletedProtectionChange,
    /// Making a private project internal.
    MakeInternal,
    /// Making an internal project private (M1 phase 8b MR7a).
    MakePrivate,
    /// Making a private project pending deletion internal.
    MakeInternalDeleted,
    OwnToken,
    ForeignToken,
    /// The caller's own notification settings (M3 N1) or another user's.
    OwnNotificationPreferences,
    ForeignNotificationPreferences,
    /// The caller's own merge request lists (plan 15 MR 6) or another user's.
    OwnUserMergeRequests,
    ForeignUserMergeRequests,
    /// The caller's own activity (plan 15 MR 8) or another user's.
    OwnUserActivity,
    ForeignUserActivity,
    /// Creating an own token with plain scopes.
    OwnTokenRequest,
    /// Creating an own token that carries the admin scope.
    OwnAdminTokenRequest,
    /// Creating a token for another user.
    ForeignTokenRequest,
    /// The caller's own sessions (`own`) or another user's (M2 MR6,
    /// ADR-0021 §17).
    Session {
        own: bool,
    },
    Instance,
    /// A path resolved (live) to a private project.
    ResolvedPrivate,
    /// A path resolved (live) to an internal project.
    ResolvedInternal,
    /// A path resolved (live) to a private project pending deletion.
    ResolvedDeleted,
    /// A `moved` tombstone pointing at a private project.
    MovedPrivate,
    /// A path resolved (live) to a group.
    ResolvedGroup,
    /// A path resolved (live) to a personal namespace.
    ResolvedUser,
    /// Nothing at the path.
    Absent,
    /// A `deleted` tombstone at the path.
    DeletedTombstone,
    /// A namespace of another kind than the route's scope (MR4).
    KindMismatch,
    /// The sign-in's admission (M2 MR4, ADR-0021 §3): the ID token's access
    /// role and the account the identity belongs to.
    Admission {
        access_role: bool,
        account: AdmissionAccount,
    },
    /// A merge request (M3 R1a, ADR-0024 §12): `matrix_review.rs`.
    Mr(crate::matrix_review::MrFixture),
    /// A discussion of a merge request (M3 R3): `matrix_review.rs`.
    Discussion(crate::matrix_review::DiscussionFixture),
    /// The user a service account route acts on (M3 S0): a service account
    /// (`human: false`) or a person, active or disabled.
    ServiceAccountTarget {
        human: bool,
        active: bool,
    },
    /// A token request for such a user (M3 S0), with the `admin` scope or
    /// with `read_api` alone.
    ServiceAccountTokenRequest {
        human: bool,
        active: bool,
        admin_scope: bool,
    },
    /// A check run report (M3 C1): the name's namespace, whether the caller
    /// holds a grant of it, on a project of `project`'s kind.
    CheckRun {
        namespace: forge_core::CheckNamespace,
        granted: bool,
        project: crate::matrix_review::MrProject,
    },
    /// A report of `ci/pipeline` (M4 C5): reserved for forge's worker
    /// although the caller may hold a grant of `ci` on a private project.
    CiPipelineCheckRun {
        granted: bool,
    },
    /// A CI output (M4 C5, `matrix_rows_ci_outputs.rs`) of a private
    /// project (`internal`: an internal one) with the pipeline's mark and
    /// the default branch's tip mark.
    CiOutput {
        internal: bool,
        pipeline_mark: crate::model::CopyMark,
        tip_mark: crate::model::CopyMark,
        /// The request declared an agent audience (`gstn mcp`).
        declared: bool,
    },
    /// An integration's feed (M3 I1c): the caller is its service account
    /// (`own`), it is enabled, the request carried `Forge-Act-As`.
    IntegrationFeed {
        own: bool,
        enabled: bool,
        act_as: bool,
    },
    /// One bound project's events of `class` (M3 I1c) on a project of
    /// `project`'s kind.
    ProjectEvents {
        class: forge_core::VisibilityClass,
        project: crate::matrix_review::MrProject,
    },
    /// A resource as an act-as request reaches it (M3 S2):
    /// `matrix_rows_act_as.rs`.
    ActAs(crate::matrix_rows_act_as::Scene),
    /// The act-as confirmation of a merge request (M3 S2).
    Confirm(crate::matrix_rows_act_as::ConfirmScene),
    /// A binding as an act-as request reaches it (M3 X3):
    /// `matrix_rows_bindings.rs`.
    ActAsBinding(crate::matrix_rows_bindings::BindingScene),
    /// The act-as confirmation of a binding intent (M3 X3).
    BindingConfirm(crate::matrix_rows_bindings::BindingConfirmScene),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expect {
    Allow,
    Deny(ErrorCode),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Row {
    pub action: Action,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the matrix tests"))]
    pub actor: Actor,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the matrix tests"))]
    pub fixture: Fixture,
    pub expect: Expect,
}

#[cfg(test)]
pub(crate) const ME: UserId = UserId::new(1);
#[cfg(test)]
pub(crate) const OTHER: UserId = UserId::new(2);

#[cfg(test)]
impl Actor {
    pub(crate) fn principal(self) -> Principal {
        let session = |admin, active| {
            Principal::authenticated(ME, admin, active, Credential::Session, AccountKind::Human)
        };
        let token_of = |kind, admin, scopes: &[Scope]| {
            Principal::authenticated(ME, admin, true, Credential::Token(scopes.to_vec()), kind)
        };
        let token = |admin, scopes: &[Scope]| token_of(AccountKind::Human, admin, scopes);
        match self {
            Actor::Anonymous => Principal::anonymous(),
            Actor::Disabled => session(false, false),
            Actor::DisabledAdmin => session(true, false),
            Actor::Admin => session(true, true),
            Actor::DevTokenReadRepository => token(false, &[Scope::ReadRepository]),
            Actor::DevTokenWriteRepository => token(false, &[Scope::WriteRepository]),
            Actor::AgentTokenWriteRepository => {
                token(false, &[Scope::WriteRepository]).as_agent(true)
            }
            Actor::DevTokenApi => token(false, &[Scope::Api]),
            Actor::AgentTokenApi | Actor::AgentOwnerTokenApi => {
                token(false, &[Scope::Api]).as_agent(true)
            }
            Actor::OwnerTokenReadApi | Actor::NonMemberTokenReadApi | Actor::GuestTokenReadApi => {
                token(false, &[Scope::ReadApi])
            }
            Actor::AgentTokenReadApi => token(false, &[Scope::ReadApi]).as_agent(true),
            Actor::AdminTokenApi => token(true, &[Scope::Api]),
            Actor::AdminTokenAdmin => token(true, &[Scope::Api, Scope::Admin]),
            Actor::AgentAdminTokenAdmin => token(true, &[Scope::Api, Scope::Admin]).as_agent(true),
            Actor::AgentMaintainerTokenApi => token(false, &[Scope::Api]).as_agent(true),
            Actor::MaintainerTokenApi => token(false, &[Scope::Api]),
            Actor::ServiceAccountTokenApi => {
                token_of(AccountKind::ServiceAccount, false, &[Scope::Api])
            }
            Actor::BotTokenApi => token_of(AccountKind::Bot, false, &[Scope::Api]),
            Actor::OwnerTokenApi => token(false, &[Scope::Api]),
            Actor::ServiceAccountOwnerTokenApi | Actor::ServiceAccountMaintainerTokenApi => {
                token_of(AccountKind::ServiceAccount, false, &[Scope::Api])
            }
            Actor::BotOwnerTokenApi | Actor::BotMaintainerTokenApi => {
                token_of(AccountKind::Bot, false, &[Scope::Api])
            }
            Actor::ServiceAccountAdminTokenAdmin => token_of(
                AccountKind::ServiceAccount,
                true,
                &[Scope::Api, Scope::Admin],
            ),
            Actor::BotAdminTokenAdmin => {
                token_of(AccountKind::Bot, true, &[Scope::Api, Scope::Admin])
            }
            Actor::ServiceAccountAdminSession => Principal::authenticated(
                ME,
                true,
                true,
                Credential::Session,
                AccountKind::ServiceAccount,
            ),
            Actor::BotAdminSession => {
                Principal::authenticated(ME, true, true, Credential::Session, AccountKind::Bot)
            }
            Actor::DevTokenMergeExecute => token(false, &[Scope::MergeExecute]),
            Actor::AgentTokenMergeExecute => token(false, &[Scope::MergeExecute]).as_agent(true),
            Actor::ServiceAccountTokenMergeExecute => {
                token_of(AccountKind::ServiceAccount, false, &[Scope::MergeExecute])
            }
            Actor::BotTokenMergeExecute => {
                token_of(AccountKind::Bot, false, &[Scope::MergeExecute])
            }
            Actor::AdminTokenMergeExecuteAdmin => token(true, &[Scope::MergeExecute, Scope::Admin]),
            Actor::AdminTokenMergeExecute => token(true, &[Scope::MergeExecute]),
            Actor::DevTokenMergeApprove => token(false, &[Scope::MergeApprove]),
            Actor::AgentTokenMergeApprove => token(false, &[Scope::MergeApprove]).as_agent(true),
            Actor::ServiceAccountTokenMergeApprove => {
                token_of(AccountKind::ServiceAccount, false, &[Scope::MergeApprove])
            }
            Actor::BotTokenMergeApprove => {
                token_of(AccountKind::Bot, false, &[Scope::MergeApprove])
            }
            Actor::MaintainerTokenReadApi => token(false, &[Scope::ReadApi]),
            Actor::AdminTokenMergeApproveAdmin => token(true, &[Scope::MergeApprove, Scope::Admin]),
            Actor::AdminTokenReadApiAdmin => token(true, &[Scope::ReadApi, Scope::Admin]),
            Actor::ServiceAccountNonMemberTokenReadApi => {
                token_of(AccountKind::ServiceAccount, false, &[Scope::ReadApi])
            }
            Actor::DevTokenUserEmail => token(false, &[Scope::UserEmail]),
            Actor::ServiceAccountGuestTokenRead => token_of(
                AccountKind::ServiceAccount,
                false,
                &[Scope::ReadApi, Scope::ReadRepository],
            ),
            Actor::BotNonMemberTokenReadApi => token_of(AccountKind::Bot, false, &[Scope::ReadApi]),
            Actor::ServiceAccountTokenReadApi => {
                token_of(AccountKind::ServiceAccount, false, &[Scope::ReadApi])
            }
            Actor::ServiceAccountReporterTokenApi => {
                token_of(AccountKind::ServiceAccount, false, &[Scope::Api])
            }
            Actor::ServiceAccountAgentTokenReadApi => {
                token_of(AccountKind::ServiceAccount, false, &[Scope::ReadApi]).as_agent(true)
            }
            Actor::ServiceAccountDisabledTokenReadApi => Principal::authenticated(
                ME,
                false,
                false,
                Credential::Token(vec![Scope::ReadApi]),
                AccountKind::ServiceAccount,
            ),
            Actor::AgentTokenReadRepository => {
                token(false, &[Scope::ReadRepository]).as_agent(true)
            }
            Actor::ServiceAccountTokenReadRepository => {
                token_of(AccountKind::ServiceAccount, false, &[Scope::ReadRepository])
            }
            Actor::BotTokenReadRepository => {
                token_of(AccountKind::Bot, false, &[Scope::ReadRepository])
            }
            Actor::ServiceAccountTokenActAs => {
                token_of(AccountKind::ServiceAccount, false, &[Scope::ActAs])
            }
            Actor::ServiceAccountAgentTokenActAs => {
                token_of(AccountKind::ServiceAccount, false, &[Scope::ActAs]).as_agent(true)
            }
            Actor::ActAs(via) => via.principal(),
            Actor::Runner => crate::matrix_rows_ci::runner(),
            Actor::Job(state) => crate::matrix_rows_ci::job(state),
            Actor::NonMember
            | Actor::Guest
            | Actor::Reporter
            | Actor::Developer
            | Actor::Maintainer
            | Actor::Owner => session(false, true),
        }
    }

    /// The actor's effective role on the row's project or group.
    pub(crate) fn role(self) -> Option<Role> {
        match self {
            Actor::Guest | Actor::GuestTokenReadApi | Actor::ServiceAccountGuestTokenRead => {
                Some(Role::Guest)
            }
            Actor::Reporter | Actor::ServiceAccountReporterTokenApi => Some(Role::Reporter),
            Actor::Developer
            | Actor::DevTokenReadRepository
            | Actor::DevTokenWriteRepository
            | Actor::AgentTokenWriteRepository
            | Actor::AgentTokenReadApi
            | Actor::AgentTokenApi
            | Actor::DevTokenApi
            | Actor::ServiceAccountTokenApi
            | Actor::BotTokenApi
            | Actor::DevTokenMergeExecute
            | Actor::AgentTokenMergeExecute
            | Actor::ServiceAccountTokenMergeExecute
            | Actor::BotTokenMergeExecute
            | Actor::DevTokenMergeApprove
            | Actor::AgentTokenMergeApprove
            | Actor::ServiceAccountTokenMergeApprove
            | Actor::BotTokenMergeApprove
            | Actor::DevTokenUserEmail
            | Actor::ServiceAccountTokenReadApi
            | Actor::ServiceAccountAgentTokenReadApi
            | Actor::ServiceAccountDisabledTokenReadApi
            | Actor::AgentTokenReadRepository
            | Actor::ServiceAccountTokenReadRepository
            | Actor::BotTokenReadRepository => Some(Role::Developer),
            Actor::ServiceAccountTokenActAs | Actor::ServiceAccountAgentTokenActAs => {
                Some(Role::Reporter)
            }
            Actor::ActAs(via) => via.person,
            // No membership: the job arm decides on the token alone.
            Actor::Runner | Actor::Job(_) => None,
            Actor::Maintainer
            | Actor::AgentMaintainerTokenApi
            | Actor::MaintainerTokenApi
            | Actor::ServiceAccountMaintainerTokenApi
            | Actor::BotMaintainerTokenApi
            | Actor::MaintainerTokenReadApi => Some(Role::Maintainer),
            Actor::Owner
            | Actor::OwnerTokenReadApi
            | Actor::AgentOwnerTokenApi
            | Actor::OwnerTokenApi
            | Actor::ServiceAccountOwnerTokenApi
            | Actor::BotOwnerTokenApi => Some(Role::Owner),
            Actor::Anonymous
            | Actor::Disabled
            | Actor::DisabledAdmin
            | Actor::NonMember
            | Actor::NonMemberTokenReadApi
            | Actor::Admin
            | Actor::AdminTokenApi
            | Actor::AdminTokenAdmin
            | Actor::AgentAdminTokenAdmin
            | Actor::ServiceAccountAdminTokenAdmin
            | Actor::ServiceAccountAdminSession
            | Actor::BotAdminTokenAdmin
            | Actor::BotAdminSession
            | Actor::AdminTokenMergeExecuteAdmin
            | Actor::AdminTokenMergeExecute
            | Actor::AdminTokenMergeApproveAdmin
            | Actor::AdminTokenReadApiAdmin
            | Actor::ServiceAccountNonMemberTokenReadApi
            | Actor::BotNonMemberTokenReadApi => None,
        }
    }

    /// The person with the same role, scopes and administrator flag as a
    /// service account / bot actor (`None` for every other actor): the
    /// matrix tests check that each of their denied people-only rows is
    /// allowed for the twin, so the denial comes from the account kind.
    pub(crate) fn human_twin(self) -> Option<Actor> {
        match self {
            Actor::ServiceAccountTokenApi | Actor::BotTokenApi => Some(Actor::DevTokenApi),
            Actor::ServiceAccountOwnerTokenApi | Actor::BotOwnerTokenApi => {
                Some(Actor::OwnerTokenApi)
            }
            Actor::ServiceAccountMaintainerTokenApi | Actor::BotMaintainerTokenApi => {
                Some(Actor::MaintainerTokenApi)
            }
            Actor::ServiceAccountAdminTokenAdmin | Actor::BotAdminTokenAdmin => {
                Some(Actor::AdminTokenAdmin)
            }
            Actor::ServiceAccountAdminSession | Actor::BotAdminSession => Some(Actor::Admin),
            Actor::ServiceAccountTokenMergeExecute | Actor::BotTokenMergeExecute => {
                Some(Actor::DevTokenMergeExecute)
            }
            Actor::ServiceAccountTokenMergeApprove | Actor::BotTokenMergeApprove => {
                Some(Actor::DevTokenMergeApprove)
            }
            Actor::ServiceAccountNonMemberTokenReadApi => Some(Actor::NonMemberTokenReadApi),
            Actor::Anonymous
            | Actor::Disabled
            | Actor::DisabledAdmin
            | Actor::NonMember
            | Actor::Guest
            | Actor::Reporter
            | Actor::Developer
            | Actor::Maintainer
            | Actor::Owner
            | Actor::Admin
            | Actor::DevTokenReadRepository
            | Actor::DevTokenWriteRepository
            | Actor::AgentTokenWriteRepository
            | Actor::AgentTokenReadApi
            | Actor::AgentTokenApi
            | Actor::AgentOwnerTokenApi
            | Actor::AgentAdminTokenAdmin
            | Actor::AgentMaintainerTokenApi
            | Actor::MaintainerTokenApi
            | Actor::DevTokenApi
            | Actor::OwnerTokenReadApi
            | Actor::NonMemberTokenReadApi
            | Actor::GuestTokenReadApi
            | Actor::AdminTokenApi
            | Actor::AdminTokenAdmin
            | Actor::OwnerTokenApi
            | Actor::DevTokenMergeExecute
            | Actor::AgentTokenMergeExecute
            | Actor::AdminTokenMergeExecuteAdmin
            | Actor::AdminTokenMergeExecute
            | Actor::DevTokenMergeApprove
            | Actor::AgentTokenMergeApprove
            | Actor::MaintainerTokenReadApi
            | Actor::AdminTokenMergeApproveAdmin
            | Actor::AdminTokenReadApiAdmin
            | Actor::DevTokenUserEmail
            | Actor::ServiceAccountGuestTokenRead
            | Actor::BotNonMemberTokenReadApi
            | Actor::ServiceAccountTokenReadApi
            | Actor::ServiceAccountReporterTokenApi
            | Actor::ServiceAccountAgentTokenReadApi
            | Actor::ServiceAccountDisabledTokenReadApi
            | Actor::ServiceAccountTokenActAs
            | Actor::ServiceAccountAgentTokenActAs
            | Actor::AgentTokenReadRepository
            | Actor::ServiceAccountTokenReadRepository
            | Actor::BotTokenReadRepository
            | Actor::ActAs(_)
            | Actor::Runner
            | Actor::Job(_) => None,
        }
    }

    /// The account kind the actor's principal carries.
    pub(crate) fn kind(self) -> AccountKind {
        self.principal().account_kind()
    }
}

#[cfg(test)]
impl Fixture {
    pub(crate) fn resource(self, actor: Actor) -> Resource {
        let role = actor.role();
        let private = ProjectView {
            visibility: Visibility::Private,
            role,
            pending_deletion: false,
        };
        let internal = ProjectView {
            visibility: Visibility::Internal,
            ..private
        };
        let deleted = ProjectView {
            pending_deletion: true,
            ..private
        };
        let member = |scope, change| Resource::Membership {
            scope,
            change,
            removes_last_owner: false,
        };
        let resolved = |via, view| Resource::Resolved { via, view };
        let transfer = |source, target| Resource::Transfer { source, target };
        let group_transfer = |target| Resource::GroupTransfer {
            source_role: role,
            target,
        };
        let hidden = TransferTarget::Group {
            role: None,
            visible: false,
        };
        let branch_ff = |project| Resource::Ref {
            project,
            name: ref_name(RefKind::Branch),
            update: RefUpdate::FastForward,
            protection: None,
        };
        match self {
            Fixture::PrivateProject => Resource::Project(private),
            Fixture::InternalProject => Resource::Project(internal),
            Fixture::DeletedProject => Resource::Project(deleted),
            Fixture::InternalDeletedProject => Resource::Project(ProjectView {
                visibility: Visibility::Internal,
                ..deleted
            }),
            Fixture::RepoCopy(mark) => Resource::RepoCopy {
                project: private,
                mark,
            },
            Fixture::InternalRepoCopy(mark) => Resource::RepoCopy {
                project: internal,
                mark,
            },
            Fixture::PinnedCopy {
                other_project,
                other_sha,
            } => crate::matrix_rows_ci::pinned_copy(other_project, other_sha),
            Fixture::RunnerJob { holder, finished } => {
                crate::matrix_rows_ci::runner_job(holder, finished)
            }
            Fixture::Ref {
                kind,
                update,
                protected,
            } => Resource::Ref {
                project: private,
                name: ref_name(kind),
                update,
                protection: RefProtection::effective(protected),
            },
            Fixture::RefNoDirectPush { update } => Resource::Ref {
                project: private,
                name: ref_name(RefKind::Branch),
                update,
                protection: RefProtection::effective([Role::Maintainer])
                    .map(RefProtection::forbidding_direct_push),
            },
            Fixture::Codeowners {
                highest,
                change,
                deleted: gone,
            } => Resource::CodeownersEnforcement {
                project: if gone { deleted } else { private },
                highest_protection: highest,
                change,
            },
            Fixture::SettingsChange {
                highest,
                loosens,
                project,
            } => Resource::MergeSettingsChange {
                project: match project {
                    crate::matrix_review::MrProject::Private => private,
                    crate::matrix_review::MrProject::Internal => internal,
                    crate::matrix_review::MrProject::Deleted => deleted,
                },
                highest_protection: highest,
                loosens,
            },
            Fixture::InternalRef => branch_ff(internal),
            Fixture::DeletedRef => branch_ff(deleted),
            Fixture::Group => Resource::Group { role },
            Fixture::OwnNamespace => Resource::Namespace {
                owner: ME,
                owner_kind: actor.kind(),
            },
            Fixture::ForeignNamespace => Resource::Namespace {
                owner: OTHER,
                owner_kind: AccountKind::Human,
            },
            Fixture::ServiceAccountNamespace => Resource::Namespace {
                owner: OTHER,
                owner_kind: AccountKind::ServiceAccount,
            },
            Fixture::TransferToServiceAccountNamespace => transfer(
                private,
                TransferTarget::PersonalNamespace {
                    owner: OTHER,
                    owner_kind: AccountKind::ServiceAccount,
                },
            ),
            Fixture::Transfer { target } => transfer(
                private,
                TransferTarget::Group {
                    role: target,
                    visible: true,
                },
            ),
            Fixture::TransferHiddenTarget => transfer(private, hidden),
            Fixture::TransferToOwnNamespace => transfer(
                private,
                TransferTarget::PersonalNamespace {
                    owner: ME,
                    owner_kind: actor.kind(),
                },
            ),
            Fixture::TransferToForeignNamespace => transfer(
                private,
                TransferTarget::PersonalNamespace {
                    owner: OTHER,
                    owner_kind: AccountKind::Human,
                },
            ),
            Fixture::TransferToTopLevel => transfer(private, TransferTarget::TopLevel),
            Fixture::TransferDeletedSource => transfer(
                deleted,
                TransferTarget::Group {
                    role: Some(Role::Owner),
                    visible: true,
                },
            ),
            Fixture::GroupTransfer { target } => group_transfer(TransferTarget::Group {
                role: target,
                visible: true,
            }),
            Fixture::GroupTransferHiddenTarget => group_transfer(hidden),
            Fixture::GroupTransferToTopLevel => group_transfer(TransferTarget::TopLevel),
            Fixture::GroupTransferToNamespace => {
                group_transfer(TransferTarget::PersonalNamespace {
                    owner: ME,
                    owner_kind: actor.kind(),
                })
            }
            Fixture::TransferRename => {
                transfer(private, TransferTarget::CurrentParent { top_level: false })
            }
            Fixture::GroupTransferRename => {
                group_transfer(TransferTarget::CurrentParent { top_level: false })
            }
            Fixture::GroupTransferRenameTopLevel => {
                group_transfer(TransferTarget::CurrentParent { top_level: true })
            }
            Fixture::UserRenameOther => Resource::UserRename { target_user: OTHER },
            Fixture::UserRenameSelf => Resource::UserRename { target_user: ME },
            Fixture::ReleaseMoved { visible } => Resource::Tombstone(TombstoneRelease::Moved {
                target_owner_role: role,
                target_visible: visible,
            }),
            Fixture::ReleaseDeleted { recorded } => {
                Resource::Tombstone(TombstoneRelease::Deleted {
                    recorded_owner: Some(if recorded { ME } else { OTHER }),
                    caller_parent_role: role,
                    has_parent: true,
                })
            }
            Fixture::ReleaseDeletedNoRole => Resource::Tombstone(TombstoneRelease::Deleted {
                recorded_owner: Some(ME),
                caller_parent_role: None,
                has_parent: true,
            }),
            Fixture::ReleaseTopLevel => Resource::Tombstone(TombstoneRelease::Deleted {
                recorded_owner: Some(ME),
                caller_parent_role: None,
                has_parent: false,
            }),
            Fixture::ProjectMember(change) => member(MembershipScope::Project(private), change),
            Fixture::InternalProjectMember(change) => {
                member(MembershipScope::Project(internal), change)
            }
            Fixture::DeletedProjectMember(change) => {
                member(MembershipScope::Project(deleted), change)
            }
            Fixture::GroupMember(change) => member(MembershipScope::Group { role }, change),
            Fixture::GroupLastOwnerRemoval => Resource::Membership {
                scope: MembershipScope::Group { role },
                change: MembershipChange::Remove { from: Role::Owner },
                removes_last_owner: true,
            },
            Fixture::LastOwnerRemoval => Resource::Membership {
                scope: MembershipScope::Project(private),
                change: MembershipChange::Remove { from: Role::Owner },
                removes_last_owner: true,
            },
            Fixture::LastOwnerDemotion => Resource::Membership {
                scope: MembershipScope::Project(private),
                change: MembershipChange::Change {
                    from: Role::Owner,
                    to: Role::Maintainer,
                },
                removes_last_owner: true,
            },
            Fixture::GroupLastOwnerDemotion => Resource::Membership {
                scope: MembershipScope::Group { role },
                change: MembershipChange::Change {
                    from: Role::Owner,
                    to: Role::Maintainer,
                },
                removes_last_owner: true,
            },
            Fixture::MissingMember(to) => member(
                MembershipScope::Project(private),
                MembershipChange::Missing { to },
            ),
            Fixture::GroupMissingMember(to) => member(
                MembershipScope::Group { role },
                MembershipChange::Missing { to },
            ),
            Fixture::SubgroupLastDirectOwner(to) => member(
                MembershipScope::Group { role },
                match to {
                    None => MembershipChange::Remove { from: Role::Owner },
                    Some(to) => MembershipChange::Change {
                        from: Role::Owner,
                        to,
                    },
                },
            ),
            Fixture::ProtectionChange { from, to } => Resource::RefProtectionChange {
                project: private,
                from,
                to,
            },
            Fixture::InternalProtectionChange => Resource::RefProtectionChange {
                project: internal,
                from: None,
                to: Some(Role::Maintainer),
            },
            Fixture::DeletedProtectionChange => Resource::RefProtectionChange {
                project: deleted,
                from: None,
                to: Some(Role::Maintainer),
            },
            Fixture::MakeInternal => Resource::VisibilityChange {
                project: private,
                to: Visibility::Internal,
            },
            Fixture::MakePrivate => Resource::VisibilityChange {
                project: internal,
                to: Visibility::Private,
            },
            Fixture::MakeInternalDeleted => Resource::VisibilityChange {
                project: deleted,
                to: Visibility::Internal,
            },
            Fixture::OwnToken => Resource::Token { owner: ME },
            Fixture::ForeignToken => Resource::Token { owner: OTHER },
            Fixture::OwnNotificationPreferences => Resource::NotificationPreferences { owner: ME },
            Fixture::ForeignNotificationPreferences => {
                Resource::NotificationPreferences { owner: OTHER }
            }
            Fixture::OwnUserMergeRequests => Resource::UserMergeRequests { owner: ME },
            Fixture::ForeignUserMergeRequests => Resource::UserMergeRequests { owner: OTHER },
            Fixture::OwnUserActivity => Resource::UserActivity { owner: ME },
            Fixture::ForeignUserActivity => Resource::UserActivity { owner: OTHER },
            Fixture::OwnTokenRequest => Resource::TokenRequest {
                owner: ME,
                scopes: vec![Scope::ReadRepository],
            },
            Fixture::OwnAdminTokenRequest => Resource::TokenRequest {
                owner: ME,
                scopes: vec![Scope::Api, Scope::Admin],
            },
            Fixture::ForeignTokenRequest => Resource::TokenRequest {
                owner: OTHER,
                scopes: vec![Scope::ReadRepository],
            },
            Fixture::Session { own } => Resource::Session {
                owner: if own { ME } else { OTHER },
            },
            Fixture::Instance => Resource::Instance,
            Fixture::ResolvedPrivate => {
                resolved(ResolvedVia::Live, NamespaceView::Project(private))
            }
            Fixture::ResolvedInternal => {
                resolved(ResolvedVia::Live, NamespaceView::Project(internal))
            }
            Fixture::ResolvedDeleted => {
                resolved(ResolvedVia::Live, NamespaceView::Project(deleted))
            }
            Fixture::MovedPrivate => resolved(ResolvedVia::Moved, NamespaceView::Project(private)),
            Fixture::ResolvedGroup => resolved(ResolvedVia::Live, NamespaceView::Group { role }),
            Fixture::ResolvedUser => resolved(ResolvedVia::Live, NamespaceView::User),
            Fixture::Absent => Resource::Unresolved(UnresolvedReason::Absent),
            Fixture::DeletedTombstone => Resource::Unresolved(UnresolvedReason::TombstoneDeleted),
            Fixture::KindMismatch => Resource::Unresolved(UnresolvedReason::KindMismatch),
            Fixture::Admission {
                access_role,
                account,
            } => Resource::Admission {
                access_role,
                account,
            },
            Fixture::CheckRun {
                namespace,
                granted,
                project,
            } => Resource::CheckRun {
                project: match project {
                    crate::matrix_review::MrProject::Private => private,
                    crate::matrix_review::MrProject::Internal => internal,
                    crate::matrix_review::MrProject::Deleted => deleted,
                },
                namespace,
                granted,
                reserved: false,
            },
            Fixture::CiPipelineCheckRun { granted } => Resource::CheckRun {
                project: private,
                namespace: forge_core::CheckNamespace::Ci,
                granted,
                reserved: true,
            },
            Fixture::CiOutput {
                internal: is_internal,
                pipeline_mark,
                tip_mark,
                declared,
            } => Resource::CiOutput {
                project: if is_internal { internal } else { private },
                pipeline_mark,
                tip_mark,
                declared_agent: declared,
            },
            Fixture::IntegrationFeed {
                own,
                enabled,
                act_as,
            } => Resource::IntegrationFeed(crate::model::IntegrationFeedView {
                own,
                enabled,
                act_as,
            }),
            Fixture::ProjectEvents { class, project } => Resource::ProjectEvents {
                project: match project {
                    crate::matrix_review::MrProject::Private => private,
                    crate::matrix_review::MrProject::Internal => internal,
                    crate::matrix_review::MrProject::Deleted => deleted,
                },
                class,
            },
            Fixture::Mr(mr) => mr.resource(role),
            Fixture::ActAs(scene) => scene.resource(role),
            Fixture::Confirm(scene) => scene.resource(role),
            Fixture::ActAsBinding(scene) => scene.resource(role),
            Fixture::BindingConfirm(scene) => scene.resource(role),
            Fixture::Discussion(d) => d.resource(role),
            Fixture::ServiceAccountTarget { human, active } => {
                Resource::ServiceAccount(service_account_view(human, active))
            }
            Fixture::ServiceAccountTokenRequest {
                human,
                active,
                admin_scope,
            } => Resource::ServiceAccountTokenRequest {
                account: service_account_view(human, active),
                scopes: if admin_scope {
                    vec![Scope::ReadApi, Scope::Admin]
                } else {
                    vec![Scope::ReadApi]
                },
            },
        }
    }
}

/// The target of a service account row (M3 S0).
#[cfg(test)]
fn service_account_view(human: bool, active: bool) -> crate::model::ServiceAccountView {
    crate::model::ServiceAccountView {
        kind: if human {
            AccountKind::Human
        } else {
            AccountKind::ServiceAccount
        },
        active,
    }
}

/// A representative ref name for each kind.
#[cfg(test)]
fn ref_name(kind: RefKind) -> forge_core::RefName {
    let name = match kind {
        RefKind::Branch => "refs/heads/main",
        RefKind::Tag => "refs/tags/v1.0",
        RefKind::Reserved => "refs/replace/0123456789abcdef0123456789abcdef01234567",
    };
    forge_core::RefName::parse(name).expect("fixture ref names are valid")
}

/// Every row: the base rows (`matrix_rows.rs`) and the discussion rows
/// (`matrix_rows_discussion.rs`, M3 R3), one list for the tests and the
/// gate summary.
pub(crate) static ROWS: std::sync::LazyLock<Vec<Row>> = std::sync::LazyLock::new(|| {
    [
        crate::matrix_rows::BASE_ROWS,
        crate::matrix_rows_discussion::DISCUSSION_ROWS,
        crate::matrix_rows_merge::MERGE_ROWS,
        crate::matrix_rows_approval::APPROVAL_ROWS,
        crate::matrix_rows_approval::APPROVAL_OTHER_ROWS,
        crate::matrix_rows_conditions::CONDITION_ROWS,
        crate::matrix_rows_people::PEOPLE_ONLY_ROWS,
        crate::matrix_rows_people::OWNER_TOKEN_ROWS,
        crate::matrix_rows_service_accounts::SERVICE_ACCOUNT_ROWS,
        crate::matrix_rows_checks::CHECK_ROWS,
        crate::matrix_rows_integrations::INTEGRATION_ROWS,
        crate::matrix_rows_work_items::WORK_ITEM_ROWS,
        crate::matrix_rows_feed::FEED_ROWS,
        crate::matrix_rows_dev_status::DEV_STATUS_ROWS,
        crate::matrix_rows_act_as::ACT_AS_ROWS,
        crate::matrix_rows_act_as::CONFIRM_ROWS,
        crate::matrix_rows_bindings::BINDING_ROWS,
        crate::matrix_rows_bindings::SEARCH_ROWS,
        crate::matrix_rows_bindings::LIST_ROWS,
        &crate::matrix_rows_act_as::generated_denies(),
        crate::matrix_rows_webhooks::WEBHOOK_ROWS.as_slice(),
        crate::matrix_rows_activity::ACTIVITY_ROWS,
        crate::matrix_rows_mirror::MIRROR_ROWS.as_slice(),
        crate::matrix_rows_doc_lint::DOC_LINT_ROWS,
        crate::matrix_rows_pipelines::PIPELINE_ROWS.as_slice(),
        crate::matrix_rows_merge_queue::MERGE_QUEUE_ROWS.as_slice(),
        crate::matrix_rows_ci_outputs::CI_OUTPUT_ROWS.as_slice(),
        crate::matrix_rows_spec_refs::SPEC_REF_ROWS,
        crate::matrix_rows_spec_refs::SPEC_READ_ROWS,
        crate::matrix_rows_spec_refs::CONTEXT_READ_ROWS,
        crate::matrix_rows_repo_copy::REPO_COPY_ROWS,
        crate::matrix_rows_ci::CI_ROWS,
        crate::matrix_rows_ci::RUNNER_ROWS.as_slice(),
        &crate::matrix_rows_ci::generated_denies(),
    ]
    .concat()
});

/// Per-action counts for the `authz-coverage` gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionSummary {
    pub action: &'static str,
    pub allow: usize,
    pub deny: usize,
    pub public: bool,
    pub git_operation: bool,
}

pub fn summary() -> Vec<ActionSummary> {
    Action::ALL
        .iter()
        .map(|&action| {
            let (allow, deny) =
                ROWS.iter()
                    .filter(|r| r.action == action)
                    .fold((0, 0), |(a, d), r| match r.expect {
                        Expect::Allow => (a + 1, d),
                        Expect::Deny(_) => (a, d + 1),
                    });
            ActionSummary {
                action: action.name(),
                allow,
                deny,
                public: action.is_public(),
                git_operation: action.is_git_operation(),
            }
        })
        .collect()
}

/// The summary as the JSON the gate reads (no serde dependency needed).
pub fn summary_json() -> String {
    let items = summary();
    let body: Vec<String> = items
        .iter()
        .map(|a| {
            format!(
                "  {{ \"action\": \"{}\", \"allow\": {}, \"deny\": {}, \"public\": {}, \"git_operation\": {} }}",
                a.action, a.allow, a.deny, a.public, a.git_operation
            )
        })
        .collect();
    format!("[\n{}\n]\n", body.join(",\n"))
}
