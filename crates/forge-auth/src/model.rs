//! The inputs of an authorization decision. Everything `authorize()` needs is
//! loaded into these values beforehand (memberships, effective roles, token
//! scopes, ref protection), so the decision itself is a pure function.
//!
//! Inputs fail closed: a `Principal` cannot be built without stating how it
//! authenticated and whether the account is active.

// Role, Visibility (D2) and Scope (D3) live in forge-core (forge-db stores and loads
// them); re-exported here so the authorization API is unchanged.
use forge_core::{RefName, UserId};
pub use forge_core::{Role, Scope, Visibility};

/// How the caller authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Interactive sign-in: every scope.
    Session,
    /// Personal access token limited to these scopes.
    Token(Vec<Scope>),
}

/// Who is asking. Built only through the constructors, which take every
/// security-relevant fact explicitly (no permissive defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    user: Option<UserId>,
    is_admin: bool,
    active: bool,
    credential: Credential,
}

impl Principal {
    /// No credentials (forge has no anonymous access; only public actions pass).
    pub fn anonymous() -> Self {
        Self {
            user: None,
            is_admin: false,
            active: false,
            credential: Credential::Session,
        }
    }

    /// A signed-in user. `active` is `false` for disabled or locked accounts.
    pub fn authenticated(
        user: UserId,
        is_admin: bool,
        active: bool,
        credential: Credential,
    ) -> Self {
        Self {
            user: Some(user),
            is_admin,
            active,
            credential,
        }
    }

    pub fn user(&self) -> Option<UserId> {
        self.user
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// Administrator rights in effect: an administrator in a session, or with
    /// a token that carries the `admin` scope.
    pub fn admin_rights(&self) -> bool {
        self.is_admin
            && match &self.credential {
                Credential::Session => true,
                Credential::Token(scopes) => scopes.contains(&Scope::Admin),
            }
    }
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

/// The namespace of a pushed ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// `refs/heads/*`
    Branch,
    /// `refs/tags/*`
    Tag,
    /// Anything else (`refs/merge-requests/*`, `refs/replace/*`, `refs/meta/*`,
    /// …): owned by forge, never pushed by clients.
    Reserved,
}

impl RefKind {
    /// The only way callers derive the kind: from the validated ref name, so
    /// no caller can default a reserved ref to `Branch`.
    pub fn of(name: &RefName) -> RefKind {
        if name.branch().is_some() {
            RefKind::Branch
        } else if name.tag().is_some() {
            RefKind::Tag
        } else {
            RefKind::Reserved
        }
    }
}

/// Protection that applies to the ref being pushed (`None` = unprotected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefProtection {
    /// Minimum role allowed to push (create / fast-forward). Private: the
    /// only constructor is `effective`, so "strictest rule wins" cannot be
    /// bypassed by building a protection from the first matching rule.
    push_min_role: Role,
}

impl RefProtection {
    /// The protection in effect when several rules match one ref: the
    /// **strictest** (highest minimum role). Adding a rule can therefore never
    /// loosen an existing protection (ADR-0010 amendment). The lookup layer
    /// must build the push resource only through this function, matching rule
    /// patterns case-insensitively (`MAIN` and `main` collide on
    /// case-insensitive file systems).
    pub fn effective(levels: impl IntoIterator<Item = Role>) -> Option<RefProtection> {
        levels
            .into_iter()
            .max()
            .map(|push_min_role| RefProtection { push_min_role })
    }

    pub fn push_min_role(&self) -> Role {
        self.push_min_role
    }
}

/// Where a membership is changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipScope {
    Project(ProjectView),
    /// The caller's effective role on the group.
    Group {
        role: Option<Role>,
    },
}

/// What is done to one membership.
///
/// `from` is the member's current **effective** role, loaded from the
/// database — never taken from the request. `Add` is only for a user who is
/// not a member yet (an existing member is a `Change`, never an upsert).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipChange {
    /// Add a new member with this role.
    Add(Role),
    /// Change a member whose current role is `from` to `to`.
    Change { from: Role, to: Role },
    /// Remove a member whose current role is `from`.
    Remove { from: Role },
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
    /// One ref update in a push. The kind is derived from `name` inside
    /// `authorize()` (callers cannot mislabel a reserved ref); `update` must
    /// come from the ancestry check of old and new object ids; `protection`
    /// from `RefProtection::effective`.
    Ref {
        project: ProjectView,
        name: RefName,
        update: RefUpdate,
        protection: Option<RefProtection>,
    },
    /// Moving a project: the caller's role on the source project and on the
    /// target group.
    Transfer {
        source: ProjectView,
        target_role: Option<Role>,
    },
    /// Changing one membership. `removes_last_owner` is computed by the
    /// lookup layer: the change would leave the project or group without an
    /// Owner.
    Membership {
        scope: MembershipScope,
        change: MembershipChange,
        removes_last_owner: bool,
    },
    /// Changing the protection of a ref pattern: the minimum push role
    /// before and after (`None` = unprotected), loaded / validated by the
    /// caller.
    RefProtectionChange {
        project: ProjectView,
        from: Option<Role>,
        to: Option<Role>,
    },
    /// Changing a project's visibility.
    VisibilityChange {
        project: ProjectView,
        to: Visibility,
    },
    /// A personal access token owned by `owner` (list / revoke).
    Token {
        owner: UserId,
    },
    /// A token about to be created for `owner` with these scopes.
    TokenRequest {
        owner: UserId,
        scopes: Vec<Scope>,
    },
    /// A path that resolved to nothing (`PathResolve`): `FG-REPO-0005` for
    /// every signed-in, active caller.
    Unresolved(UnresolvedReason),
    /// A path that resolved to a namespace (`PathResolve`), decided by
    /// whether the caller can see it — not by a read role (ADR-0015).
    Resolved {
        via: ResolvedVia,
        view: NamespaceView,
    },
}

/// Why a path resolved to nothing. Both are answered with the same
/// `FG-REPO-0005`; the difference reaches only the log's `reason`
/// (ADR-0015 "判定の順序" step 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedReason {
    /// No namespace and no tombstone at the path.
    Absent,
    /// A `deleted` tombstone, or a `moved` one whose target is gone.
    TombstoneDeleted,
}

impl UnresolvedReason {
    /// The log `reason`.
    pub fn as_str(self) -> &'static str {
        match self {
            UnresolvedReason::Absent => "absent",
            UnresolvedReason::TombstoneDeleted => "tombstone_deleted",
        }
    }
}

/// How a path reached the namespace it resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedVia {
    /// The path is the namespace's current path.
    Live,
    /// The path is a `moved` tombstone pointing at the namespace (an
    /// invisible target is logged as `tombstone_not_visible`).
    Moved,
}

/// The namespace a path resolved to, as seen by the caller. The kind is the
/// variant, so a kind and a view can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceView {
    /// A personal namespace: visible to every signed-in user.
    User,
    /// A group: visible to every signed-in user (as for `GroupRead`); `role`
    /// is the caller's effective role on it.
    Group { role: Option<Role> },
    /// A project: visible to its members (Guest included), to every
    /// signed-in user when internal, and to administrators.
    Project(ProjectView),
}

impl NamespaceView {
    /// `user` / `group` / `project` (the `namespaces.kind` values).
    pub fn kind(&self) -> &'static str {
        match self {
            NamespaceView::User => "user",
            NamespaceView::Group { .. } => "group",
            NamespaceView::Project(_) => "project",
        }
    }
}
