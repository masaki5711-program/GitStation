//! The permission matrix: the specification of `authorize()` as data.
//!
//! Every action has at least one allowed and one denied row (public actions:
//! an allowed anonymous row), and every row is asserted by the tests. Only
//! the summary is public: it feeds `Git/docs/authz/matrix.generated.json`,
//! which the `authz-coverage` gate checks against the route registry.

use forge_errors::ErrorCode;

use crate::action::Action;
use crate::model::{MembershipChange, RefKind, RefUpdate, Role};
// Turning rows into principals and resources is only needed by the tests.
#[cfg(test)]
use crate::model::{
    Credential, MembershipScope, Principal, ProjectView, RefProtection, Resource, Scope, Visibility,
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
    /// Developer with an `api` token.
    DevTokenApi,
    /// Owner with a `read_api` token.
    OwnerTokenReadApi,
    /// Administrator with an `api` token (no `admin` scope).
    AdminTokenApi,
    /// Administrator with an `api` + `admin` token.
    AdminTokenAdmin,
}

/// The resource in a row; the caller's role on it comes from the actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fixture {
    PrivateProject,
    InternalProject,
    DeletedProject,
    /// A ref update on a private project; `protected` = minimum push role.
    Ref {
        kind: RefKind,
        update: RefUpdate,
        protected: Option<Role>,
    },
    /// A branch fast-forward on an internal project.
    InternalRef,
    /// A branch fast-forward on a project pending deletion.
    DeletedRef,
    Group,
    OwnNamespace,
    ForeignNamespace,
    /// Transfer to a group where the caller has `target`.
    Transfer {
        target: Option<Role>,
    },
    ProjectMember(MembershipChange),
    InternalProjectMember(MembershipChange),
    DeletedProjectMember(MembershipChange),
    GroupMember(MembershipChange),
    /// Removing the last Owner of a private project the caller owns.
    LastOwnerRemoval,
    /// Changing a protection's minimum push role (None = unprotected).
    ProtectionChange {
        from: Option<Role>,
        to: Option<Role>,
    },
    /// Making a private project internal.
    MakeInternal,
    OwnToken,
    ForeignToken,
    /// Creating an own token with plain scopes.
    OwnTokenRequest,
    /// Creating an own token that carries the admin scope.
    OwnAdminTokenRequest,
    /// Creating a token for another user.
    ForeignTokenRequest,
    Instance,
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
        let session =
            |admin, active| Principal::authenticated(ME, admin, active, Credential::Session);
        let token = |admin, scopes: &[Scope]| {
            Principal::authenticated(ME, admin, true, Credential::Token(scopes.to_vec()))
        };
        match self {
            Actor::Anonymous => Principal::anonymous(),
            Actor::Disabled => session(false, false),
            Actor::DisabledAdmin => session(true, false),
            Actor::Admin => session(true, true),
            Actor::DevTokenReadRepository => token(false, &[Scope::ReadRepository]),
            Actor::DevTokenWriteRepository => token(false, &[Scope::WriteRepository]),
            Actor::DevTokenApi => token(false, &[Scope::Api]),
            Actor::OwnerTokenReadApi => token(false, &[Scope::ReadApi]),
            Actor::AdminTokenApi => token(true, &[Scope::Api]),
            Actor::AdminTokenAdmin => token(true, &[Scope::Api, Scope::Admin]),
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
            Actor::Guest => Some(Role::Guest),
            Actor::Reporter => Some(Role::Reporter),
            Actor::Developer
            | Actor::DevTokenReadRepository
            | Actor::DevTokenWriteRepository
            | Actor::DevTokenApi => Some(Role::Developer),
            Actor::Maintainer => Some(Role::Maintainer),
            Actor::Owner | Actor::OwnerTokenReadApi => Some(Role::Owner),
            Actor::Anonymous
            | Actor::Disabled
            | Actor::DisabledAdmin
            | Actor::NonMember
            | Actor::Admin
            | Actor::AdminTokenApi
            | Actor::AdminTokenAdmin => None,
        }
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
        let branch_ff = |project| Resource::Ref {
            project,
            kind: RefKind::Branch,
            update: RefUpdate::FastForward,
            protection: None,
        };
        match self {
            Fixture::PrivateProject => Resource::Project(private),
            Fixture::InternalProject => Resource::Project(internal),
            Fixture::DeletedProject => Resource::Project(deleted),
            Fixture::Ref {
                kind,
                update,
                protected,
            } => Resource::Ref {
                project: private,
                kind,
                update,
                protection: protected.map(|push_min_role| RefProtection { push_min_role }),
            },
            Fixture::InternalRef => branch_ff(internal),
            Fixture::DeletedRef => branch_ff(deleted),
            Fixture::Group => Resource::Group { role },
            Fixture::OwnNamespace => Resource::Namespace { owner: ME },
            Fixture::ForeignNamespace => Resource::Namespace { owner: OTHER },
            Fixture::Transfer { target } => Resource::Transfer {
                source: private,
                target_role: target,
            },
            Fixture::ProjectMember(change) => member(MembershipScope::Project(private), change),
            Fixture::InternalProjectMember(change) => {
                member(MembershipScope::Project(internal), change)
            }
            Fixture::DeletedProjectMember(change) => {
                member(MembershipScope::Project(deleted), change)
            }
            Fixture::GroupMember(change) => member(MembershipScope::Group { role }, change),
            Fixture::LastOwnerRemoval => Resource::Membership {
                scope: MembershipScope::Project(private),
                change: MembershipChange::Remove { from: Role::Owner },
                removes_last_owner: true,
            },
            Fixture::ProtectionChange { from, to } => Resource::RefProtectionChange {
                project: private,
                from,
                to,
            },
            Fixture::MakeInternal => Resource::VisibilityChange {
                project: private,
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
            Fixture::Instance => Resource::Instance,
        }
    }
}

pub(crate) use crate::matrix_rows::ROWS;

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
