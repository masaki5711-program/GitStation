//! The dispatcher's data access (ADR-0015 "判定の順序" steps 5–6, M1 phase 8
//! MR1). forge-db implements these traits (MR3) and `forge serve` injects
//! them, so forge-web never depends on forge-db (CLAUDE.md §3-9; the same
//! shape as [`crate::readiness::ReadinessProbe`] and
//! [`crate::Authenticator`]). The types here are forge-core's own: forge-core
//! sits below forge-auth, so the dispatcher builds forge-auth's
//! `ProjectView` / `Resource::Group` from [`ProjectAccess`] and the group
//! role.

use std::future::Future;
use std::pin::Pin;

use forge_errors::ForgeError;

use crate::namespace_path::{LookupPath, NamespacePath};
use crate::{GroupId, NamespaceId, ProjectId, Role, TombstoneId, UserId, Visibility};

/// The future these traits return (boxed: they are stored as trait objects).
pub type LookupFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ForgeError>> + Send + 'a>>;

/// What a namespace row belongs to (`forge.namespaces.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamespaceKind {
    User,
    Group,
    Project,
}

impl NamespaceKind {
    /// Every kind, in the order user, group, project.
    pub const ALL: &'static [NamespaceKind] = &[Self::User, Self::Group, Self::Project];

    /// The database string (`forge.namespaces.kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Group => "group",
            Self::Project => "project",
        }
    }

    /// The kind named by `raw` (exact match), `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == raw)
    }
}

/// The typed owner of a namespace: the kind and the id in one value, so
/// they cannot disagree (`forge.namespaces.kind` + `owner_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamespaceOwner {
    User(UserId),
    Group(GroupId),
    Project(ProjectId),
}

impl NamespaceOwner {
    pub fn kind(self) -> NamespaceKind {
        match self {
            Self::User(_) => NamespaceKind::User,
            Self::Group(_) => NamespaceKind::Group,
            Self::Project(_) => NamespaceKind::Project,
        }
    }

    /// The raw owner id (for logs).
    pub fn raw_id(self) -> i64 {
        match self {
            Self::User(id) => id.get(),
            Self::Group(id) => id.get(),
            Self::Project(id) => id.get(),
        }
    }
}

/// A live namespace a path leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Resolved {
    /// The `forge.namespaces` row (logged as `namespace_id`).
    pub namespace_id: NamespaceId,
    pub owner: NamespaceOwner,
}

impl Resolved {
    pub fn kind(&self) -> NamespaceKind {
        self.owner.kind()
    }
}

/// The answer of [`NamespaceResolver::resolve`] (ADR-0015 "名前の変更、移管、
/// 削除と墓標"). Only exact matches: the live path first, then a tombstone
/// with exactly that `old_path`; never a prefix match, never a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The path is a live namespace.
    Live(Resolved),
    /// A `moved` tombstone whose target is live. `current` is the target's
    /// path now, rebuilt from its id and confirmed by an exact match on the
    /// live paths (it feeds the upload-pack `302` and the SPA's
    /// `canonical_path`).
    Moved {
        resolved: Resolved,
        tombstone: TombstoneId,
        current: NamespacePath,
    },
    /// A `deleted` tombstone, or a `moved` one whose target is gone or has
    /// no current path: the path stays blocked (`FG-REPO-0005`
    /// `reason=tombstone_deleted`).
    Deleted { tombstone: TombstoneId },
    /// Nothing at that path.
    Absent,
}

/// Path → namespace, for the dispatcher (step 5) and
/// `GET /api/v1/namespaces/resolve` (ADR-0015 PathResolve).
pub trait NamespaceResolver: Send + Sync {
    /// Exact matches only (see [`Resolution`]). Absence is
    /// [`Resolution::Absent`], not an error; errors are store failures
    /// (`FG-DB-…`) or rows that cannot be read. The caller bounds the call
    /// with its own timeout. (`prefix=true` resolution arrives with the M2
    /// rescue of GitHub-shaped URLs.)
    fn resolve<'a>(&'a self, path: &'a LookupPath) -> LookupFuture<'a, Resolution>;
}

/// What forge-auth's `ProjectView` needs for one caller and one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectAccess {
    pub visibility: Visibility,
    /// The caller's native effective role (direct, ancestor groups, owner
    /// of the personal namespace); `None` = no role.
    pub role: Option<Role>,
    /// Deleted and inside the grace period (`deleted_at` set, not purged).
    pub pending_deletion: bool,
}

/// Loads the authorization inputs of a resolved namespace (step 6) for an
/// authenticated caller. Implementations issue the same statements whether
/// or not the caller may see the target (ADR-0015 step 6: an invisible and
/// an absent target answer alike, in time too).
pub trait AccessLoader: Send + Sync {
    /// `FG-REPO-0005` (`resource=project`, `reason=absent`) when the project
    /// does not exist (it was purged after resolution).
    fn project_access<'a>(
        &'a self,
        project: ProjectId,
        user: UserId,
    ) -> LookupFuture<'a, ProjectAccess>;

    /// The caller's effective role on `group` (`Resource::Group { role }`);
    /// `FG-REPO-0005` (`resource=group`, `reason=absent`) when it does not
    /// exist.
    fn group_role<'a>(&'a self, group: GroupId, user: UserId) -> LookupFuture<'a, Option<Role>>;
}
