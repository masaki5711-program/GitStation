//! The inputs of an authorization decision. Everything `authorize()` needs is
//! loaded into these values beforehand (memberships, effective roles, token
//! scopes, ref protection), so the decision itself is a pure function.

use forge_core::UserId;

/// GitLab-style roles, ordered: `Guest < Reporter < Developer < Maintainer < Owner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Guest,
    Reporter,
    Developer,
    Maintainer,
    Owner,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Guest => "guest",
            Role::Reporter => "reporter",
            Role::Developer => "developer",
            Role::Maintainer => "maintainer",
            Role::Owner => "owner",
        }
    }
}

/// What a token may be used for. Scopes only narrow a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// git clone / fetch.
    ReadRepository,
    /// git push (implies `ReadRepository`).
    WriteRepository,
    /// REST API reads.
    ReadApi,
    /// REST API reads and writes (implies `ReadApi`).
    Api,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::ReadRepository => "read_repository",
            Scope::WriteRepository => "write_repository",
            Scope::ReadApi => "read_api",
            Scope::Api => "api",
        }
    }

    /// Whether holding `self` satisfies a requirement of `required`.
    pub fn satisfies(self, required: Scope) -> bool {
        self == required
            || matches!(
                (self, required),
                (Scope::WriteRepository, Scope::ReadRepository) | (Scope::Api, Scope::ReadApi)
            )
    }
}

/// Who is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// `None` = no credentials (forge has no anonymous access).
    pub user: Option<UserId>,
    pub is_admin: bool,
    /// `false` for disabled or locked accounts.
    pub active: bool,
    /// `None` = interactive session (all scopes); `Some` = personal access token.
    pub token_scopes: Option<Vec<Scope>>,
}

impl Principal {
    pub fn anonymous() -> Self {
        Self {
            user: None,
            is_admin: false,
            active: true,
            token_scopes: None,
        }
    }

    pub fn user(id: UserId) -> Self {
        Self {
            user: Some(id),
            is_admin: false,
            active: true,
            token_scopes: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Members only.
    Private,
    /// Every signed-in user may read.
    Internal,
}

/// A project as seen by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectView {
    pub visibility: Visibility,
    /// The caller's effective role: max over direct membership and ancestor
    /// groups. `None` = not a member.
    pub role: Option<Role>,
    /// Deleted and inside the grace period.
    pub pending_deletion: bool,
}

/// What a push does to one ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefUpdate {
    Create,
    FastForward,
    /// Non-fast-forward (rewrites history).
    Force,
    Delete,
}

/// Protection that applies to the ref being pushed (`None` = unprotected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefProtection {
    /// Minimum role allowed to push (create / fast-forward).
    pub push_min_role: Role,
}

/// What the action acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// Instance-wide (user administration, top-level groups, metrics).
    Instance,
    /// A group; `role` is the caller's effective role on it.
    Group {
        role: Option<Role>,
    },
    /// A user's personal namespace.
    Namespace {
        owner: UserId,
    },
    Project(ProjectView),
    /// One ref update in a push.
    Ref {
        project: ProjectView,
        update: RefUpdate,
        protection: Option<RefProtection>,
    },
    /// Moving a project: the caller's role on the source project and on the
    /// target group.
    Transfer {
        source: ProjectView,
        target_role: Option<Role>,
    },
    /// Granting `granting` on a project or group where the caller has `role`.
    Membership {
        role: Option<Role>,
        granting: Role,
    },
    /// A personal access token owned by `owner`.
    Token {
        owner: UserId,
    },
}
