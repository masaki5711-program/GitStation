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
}

/// The resource in a row; the caller's role on it comes from the actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fixture {
    PrivateProject,
    InternalProject,
    DeletedProject,
    /// An internal project pending deletion (every signed-in user sees it).
    InternalDeletedProject,
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
            Actor::Guest | Actor::GuestTokenReadApi => Some(Role::Guest),
            Actor::Reporter => Some(Role::Reporter),
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
            | Actor::BotTokenMergeApprove => Some(Role::Developer),
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
            | Actor::AdminTokenMergeApproveAdmin => None,
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
            | Actor::AdminTokenMergeApproveAdmin => None,
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
            Fixture::OwnNamespace => Resource::Namespace { owner: ME },
            Fixture::ForeignNamespace => Resource::Namespace { owner: OTHER },
            Fixture::Transfer { target } => transfer(
                private,
                TransferTarget::Group {
                    role: target,
                    visible: true,
                },
            ),
            Fixture::TransferHiddenTarget => transfer(private, hidden),
            Fixture::TransferToOwnNamespace => {
                transfer(private, TransferTarget::PersonalNamespace { owner: ME })
            }
            Fixture::TransferToForeignNamespace => {
                transfer(private, TransferTarget::PersonalNamespace { owner: OTHER })
            }
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
                group_transfer(TransferTarget::PersonalNamespace { owner: ME })
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
            Fixture::Mr(mr) => mr.resource(role),
            Fixture::Discussion(d) => d.resource(role),
        }
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
