//! The permission matrix: the specification of `authorize()` as data.
//!
//! Every action has at least one allowed and one denied row (public actions:
//! an allowed anonymous row), and every row is asserted by the tests. The
//! summary feeds `Git/docs/authz/matrix.generated.json`, which the
//! `authz-coverage` gate checks against the route registry.

use forge_core::UserId;
use forge_errors::ErrorCode;

use crate::action::Action;
use crate::model::{
    Principal, ProjectView, RefProtection, RefUpdate, Resource, Role, Scope, Visibility,
};

/// The caller in a row. Token actors are Developers holding only those scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    Anonymous,
    Disabled,
    NonMember,
    Guest,
    Reporter,
    Developer,
    Maintainer,
    Owner,
    Admin,
    /// Developer authenticated with a `read_repository`-only token.
    DevTokenReadRepository,
    /// Developer authenticated with an `api`-only token.
    DevTokenApi,
    /// Owner authenticated with a `read_api`-only token.
    OwnerTokenReadApi,
}

/// The resource in a row; the caller's role on it comes from the actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fixture {
    PrivateProject,
    InternalProject,
    DeletedProject,
    /// A ref update; `protected` = minimum push role when protected.
    Ref {
        update: RefUpdate,
        protected: Option<Role>,
    },
    Group,
    OwnNamespace,
    ForeignNamespace,
    /// Transfer to a group where the caller has `target`.
    Transfer {
        target: Option<Role>,
    },
    /// Grant this role.
    Grant(Role),
    OwnToken,
    ForeignToken,
    Instance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    Allow,
    Deny(ErrorCode),
}

#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub action: Action,
    pub actor: Actor,
    pub fixture: Fixture,
    pub expect: Expect,
}

const ME: UserId = UserId::new(1);
const OTHER: UserId = UserId::new(2);

impl Actor {
    pub fn principal(self) -> Principal {
        let mut p = Principal::user(ME);
        match self {
            Actor::Anonymous => return Principal::anonymous(),
            Actor::Disabled => p.active = false,
            Actor::Admin => p.is_admin = true,
            Actor::DevTokenReadRepository => p.token_scopes = Some(vec![Scope::ReadRepository]),
            Actor::DevTokenApi => p.token_scopes = Some(vec![Scope::Api]),
            Actor::OwnerTokenReadApi => p.token_scopes = Some(vec![Scope::ReadApi]),
            Actor::NonMember
            | Actor::Guest
            | Actor::Reporter
            | Actor::Developer
            | Actor::Maintainer
            | Actor::Owner => {}
        }
        p
    }

    /// The actor's effective role on the row's project or group.
    pub fn role(self) -> Option<Role> {
        match self {
            Actor::Guest => Some(Role::Guest),
            Actor::Reporter => Some(Role::Reporter),
            Actor::Developer | Actor::DevTokenReadRepository | Actor::DevTokenApi => {
                Some(Role::Developer)
            }
            Actor::Maintainer => Some(Role::Maintainer),
            Actor::Owner | Actor::OwnerTokenReadApi => Some(Role::Owner),
            Actor::Anonymous | Actor::Disabled | Actor::NonMember | Actor::Admin => None,
        }
    }
}

impl Fixture {
    pub fn resource(self, actor: Actor) -> Resource {
        let role = actor.role();
        let private = ProjectView {
            visibility: Visibility::Private,
            role,
            pending_deletion: false,
        };
        match self {
            Fixture::PrivateProject => Resource::Project(private),
            Fixture::InternalProject => Resource::Project(ProjectView {
                visibility: Visibility::Internal,
                ..private
            }),
            Fixture::DeletedProject => Resource::Project(ProjectView {
                pending_deletion: true,
                ..private
            }),
            Fixture::Ref { update, protected } => Resource::Ref {
                project: private,
                update,
                protection: protected.map(|push_min_role| RefProtection { push_min_role }),
            },
            Fixture::Group => Resource::Group { role },
            Fixture::OwnNamespace => Resource::Namespace { owner: ME },
            Fixture::ForeignNamespace => Resource::Namespace { owner: OTHER },
            Fixture::Transfer { target } => Resource::Transfer {
                source: private,
                target_role: target,
            },
            Fixture::Grant(granting) => Resource::Membership { role, granting },
            Fixture::OwnToken => Resource::Token { owner: ME },
            Fixture::ForeignToken => Resource::Token { owner: OTHER },
            Fixture::Instance => Resource::Instance,
        }
    }
}

const fn row(action: Action, actor: Actor, fixture: Fixture, expect: Expect) -> Row {
    Row {
        action,
        actor,
        fixture,
        expect,
    }
}

use Action as A;
use Actor as W;
use Expect::{Allow, Deny};
use Fixture as F;

const FF: RefUpdate = RefUpdate::FastForward;

/// The matrix. Grouped by action; denied rows are first-class.
pub const ROWS: &[Row] = &[
    // RepoRead (git clone / fetch)
    row(
        A::RepoRead,
        W::Anonymous,
        F::PrivateProject,
        Deny(ErrorCode::Unauthenticated),
    ),
    row(
        A::RepoRead,
        W::Disabled,
        F::PrivateProject,
        Deny(ErrorCode::AccountDisabled),
    ),
    row(
        A::RepoRead,
        W::NonMember,
        F::PrivateProject,
        Deny(ErrorCode::NotFound),
    ),
    row(
        A::RepoRead,
        W::Guest,
        F::PrivateProject,
        Deny(ErrorCode::Forbidden),
    ),
    row(A::RepoRead, W::Reporter, F::PrivateProject, Allow),
    row(A::RepoRead, W::Developer, F::PrivateProject, Allow),
    row(A::RepoRead, W::Admin, F::PrivateProject, Allow),
    row(
        A::RepoRead,
        W::DevTokenReadRepository,
        F::PrivateProject,
        Allow,
    ),
    row(
        A::RepoRead,
        W::DevTokenApi,
        F::PrivateProject,
        Deny(ErrorCode::InsufficientScope),
    ),
    row(A::RepoRead, W::NonMember, F::InternalProject, Allow),
    row(A::RepoRead, W::Guest, F::InternalProject, Allow),
    row(
        A::RepoRead,
        W::Developer,
        F::DeletedProject,
        Deny(ErrorCode::PendingDeletion),
    ),
    // RepoPush (one ref update)
    row(
        A::RepoPush,
        W::Anonymous,
        F::Ref {
            update: FF,
            protected: None,
        },
        Deny(ErrorCode::Unauthenticated),
    ),
    row(
        A::RepoPush,
        W::NonMember,
        F::Ref {
            update: FF,
            protected: None,
        },
        Deny(ErrorCode::NotFound),
    ),
    row(
        A::RepoPush,
        W::Reporter,
        F::Ref {
            update: FF,
            protected: None,
        },
        Deny(ErrorCode::Forbidden),
    ),
    row(
        A::RepoPush,
        W::Developer,
        F::Ref {
            update: FF,
            protected: None,
        },
        Allow,
    ),
    row(
        A::RepoPush,
        W::Developer,
        F::Ref {
            update: RefUpdate::Create,
            protected: None,
        },
        Allow,
    ),
    row(
        A::RepoPush,
        W::Developer,
        F::Ref {
            update: RefUpdate::Force,
            protected: None,
        },
        Allow,
    ),
    row(
        A::RepoPush,
        W::Developer,
        F::Ref {
            update: RefUpdate::Delete,
            protected: None,
        },
        Allow,
    ),
    row(
        A::RepoPush,
        W::DevTokenReadRepository,
        F::Ref {
            update: FF,
            protected: None,
        },
        Deny(ErrorCode::InsufficientScope),
    ),
    row(
        A::RepoPush,
        W::Developer,
        F::Ref {
            update: FF,
            protected: Some(Role::Maintainer),
        },
        Deny(ErrorCode::ProtectedRefPushNotAllowed),
    ),
    row(
        A::RepoPush,
        W::Developer,
        F::Ref {
            update: FF,
            protected: Some(Role::Developer),
        },
        Allow,
    ),
    row(
        A::RepoPush,
        W::Maintainer,
        F::Ref {
            update: FF,
            protected: Some(Role::Maintainer),
        },
        Allow,
    ),
    row(
        A::RepoPush,
        W::Admin,
        F::Ref {
            update: FF,
            protected: Some(Role::Owner),
        },
        Allow,
    ),
    row(
        A::RepoPush,
        W::Owner,
        F::Ref {
            update: RefUpdate::Force,
            protected: Some(Role::Maintainer),
        },
        Deny(ErrorCode::ProtectedRefForcePush),
    ),
    row(
        A::RepoPush,
        W::Admin,
        F::Ref {
            update: RefUpdate::Force,
            protected: Some(Role::Maintainer),
        },
        Deny(ErrorCode::ProtectedRefForcePush),
    ),
    row(
        A::RepoPush,
        W::Owner,
        F::Ref {
            update: RefUpdate::Delete,
            protected: Some(Role::Maintainer),
        },
        Deny(ErrorCode::ProtectedRefDeletion),
    ),
    row(
        A::RepoPush,
        W::Admin,
        F::Ref {
            update: RefUpdate::Delete,
            protected: Some(Role::Maintainer),
        },
        Deny(ErrorCode::ProtectedRefDeletion),
    ),
    // ProjectRead (API)
    row(A::ProjectRead, W::Reporter, F::PrivateProject, Allow),
    row(
        A::ProjectRead,
        W::Guest,
        F::PrivateProject,
        Deny(ErrorCode::Forbidden),
    ),
    row(
        A::ProjectRead,
        W::NonMember,
        F::PrivateProject,
        Deny(ErrorCode::NotFound),
    ),
    row(A::ProjectRead, W::NonMember, F::InternalProject, Allow),
    row(
        A::ProjectRead,
        W::DevTokenReadRepository,
        F::PrivateProject,
        Deny(ErrorCode::InsufficientScope),
    ),
    row(
        A::ProjectRead,
        W::OwnerTokenReadApi,
        F::PrivateProject,
        Allow,
    ),
    // ProjectCreate
    row(A::ProjectCreate, W::Maintainer, F::Group, Allow),
    row(
        A::ProjectCreate,
        W::Developer,
        F::Group,
        Deny(ErrorCode::Forbidden),
    ),
    row(A::ProjectCreate, W::Admin, F::Group, Allow),
    row(A::ProjectCreate, W::NonMember, F::OwnNamespace, Allow),
    row(
        A::ProjectCreate,
        W::Developer,
        F::ForeignNamespace,
        Deny(ErrorCode::Forbidden),
    ),
    // ProjectUpdate / ProtectedRefManage
    row(A::ProjectUpdate, W::Maintainer, F::PrivateProject, Allow),
    row(
        A::ProjectUpdate,
        W::Developer,
        F::PrivateProject,
        Deny(ErrorCode::Forbidden),
    ),
    row(
        A::ProjectUpdate,
        W::OwnerTokenReadApi,
        F::PrivateProject,
        Deny(ErrorCode::InsufficientScope),
    ),
    row(
        A::ProtectedRefManage,
        W::Maintainer,
        F::PrivateProject,
        Allow,
    ),
    row(
        A::ProtectedRefManage,
        W::Developer,
        F::PrivateProject,
        Deny(ErrorCode::Forbidden),
    ),
    // ProjectTransfer
    row(
        A::ProjectTransfer,
        W::Owner,
        F::Transfer {
            target: Some(Role::Maintainer),
        },
        Allow,
    ),
    row(
        A::ProjectTransfer,
        W::Owner,
        F::Transfer {
            target: Some(Role::Developer),
        },
        Deny(ErrorCode::Forbidden),
    ),
    row(
        A::ProjectTransfer,
        W::Maintainer,
        F::Transfer {
            target: Some(Role::Owner),
        },
        Deny(ErrorCode::Forbidden),
    ),
    // ProjectDelete / ProjectRestore
    row(A::ProjectDelete, W::Owner, F::PrivateProject, Allow),
    row(
        A::ProjectDelete,
        W::Maintainer,
        F::PrivateProject,
        Deny(ErrorCode::Forbidden),
    ),
    row(
        A::ProjectDelete,
        W::Owner,
        F::DeletedProject,
        Deny(ErrorCode::PendingDeletion),
    ),
    row(A::ProjectRestore, W::Owner, F::DeletedProject, Allow),
    row(
        A::ProjectRestore,
        W::Maintainer,
        F::DeletedProject,
        Deny(ErrorCode::Forbidden),
    ),
    // MemberManage
    row(
        A::MemberManage,
        W::Maintainer,
        F::Grant(Role::Developer),
        Allow,
    ),
    row(
        A::MemberManage,
        W::Maintainer,
        F::Grant(Role::Owner),
        Deny(ErrorCode::Forbidden),
    ),
    row(A::MemberManage, W::Owner, F::Grant(Role::Owner), Allow),
    row(
        A::MemberManage,
        W::Developer,
        F::Grant(Role::Guest),
        Deny(ErrorCode::Forbidden),
    ),
    // Groups
    row(A::GroupRead, W::NonMember, F::Group, Allow),
    row(
        A::GroupRead,
        W::Anonymous,
        F::Group,
        Deny(ErrorCode::Unauthenticated),
    ),
    row(A::GroupCreateTopLevel, W::Admin, F::Instance, Allow),
    row(
        A::GroupCreateTopLevel,
        W::Owner,
        F::Instance,
        Deny(ErrorCode::Forbidden),
    ),
    row(A::SubgroupCreate, W::Owner, F::Group, Allow),
    row(
        A::SubgroupCreate,
        W::Maintainer,
        F::Group,
        Deny(ErrorCode::Forbidden),
    ),
    row(A::GroupUpdate, W::Owner, F::Group, Allow),
    row(
        A::GroupUpdate,
        W::Maintainer,
        F::Group,
        Deny(ErrorCode::Forbidden),
    ),
    // Tokens
    row(A::TokenCreate, W::Reporter, F::OwnToken, Allow),
    row(
        A::TokenCreate,
        W::Admin,
        F::ForeignToken,
        Deny(ErrorCode::Forbidden),
    ),
    row(A::TokenList, W::Reporter, F::OwnToken, Allow),
    row(
        A::TokenList,
        W::Reporter,
        F::ForeignToken,
        Deny(ErrorCode::Forbidden),
    ),
    row(A::TokenRevoke, W::Reporter, F::OwnToken, Allow),
    row(
        A::TokenRevoke,
        W::Reporter,
        F::ForeignToken,
        Deny(ErrorCode::Forbidden),
    ),
    row(A::TokenRevoke, W::Admin, F::ForeignToken, Allow),
    // Users and audit
    row(A::UserCreate, W::Admin, F::Instance, Allow),
    row(
        A::UserCreate,
        W::Owner,
        F::Instance,
        Deny(ErrorCode::Forbidden),
    ),
    row(A::UserDisable, W::Admin, F::Instance, Allow),
    row(
        A::UserDisable,
        W::Maintainer,
        F::Instance,
        Deny(ErrorCode::Forbidden),
    ),
    row(A::AuditRead, W::Admin, F::Instance, Allow),
    row(
        A::AuditRead,
        W::Owner,
        F::Instance,
        Deny(ErrorCode::Forbidden),
    ),
    row(A::AuditRead, W::Owner, F::Group, Allow),
    row(
        A::AuditRead,
        W::Maintainer,
        F::Group,
        Deny(ErrorCode::Forbidden),
    ),
    // Operations
    row(A::Health, W::Anonymous, F::Instance, Allow),
    row(A::Metrics, W::Admin, F::Instance, Allow),
    row(
        A::Metrics,
        W::Owner,
        F::Instance,
        Deny(ErrorCode::Forbidden),
    ),
];

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
            let rows = ROWS.iter().filter(|r| r.action == action);
            let (allow, deny) = rows.fold((0, 0), |(a, d), r| match r.expect {
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
    let mut s = String::from("[\n");
    let items = summary();
    for (i, a) in items.iter().enumerate() {
        s.push_str(&format!(
            "  {{ \"action\": \"{}\", \"allow\": {}, \"deny\": {}, \"public\": {}, \"git_operation\": {} }}{}\n",
            a.action,
            a.allow,
            a.deny,
            a.public,
            a.git_operation,
            if i + 1 < items.len() { "," } else { "" }
        ));
    }
    s.push_str("]\n");
    s
}
