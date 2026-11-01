//! The dispatcher's data access (ADR-0015 "判定の順序" steps 5–6, M1 phase 8
//! MR1). forge-db implements these traits (MR3) and `forge serve` injects
//! them, so forge-web never depends on forge-db (CLAUDE.md §3-9; the same
//! shape as [`crate::readiness::ReadinessProbe`] and
//! [`crate::Authenticator`]). The types here are forge-core's own: forge-core
//! sits below forge-auth, so forge-web converts them into forge-auth's
//! resource contexts.
//!
//! **One call answers both "what is at this path" and "what may this viewer
//! do there"**: resolution and the authorization inputs come from the same
//! call, so an absent, a private and a visible target cost the same
//! statements (ADR-0015 step 6: no timing oracle).

use std::future::Future;
use std::pin::Pin;

use forge_errors::ForgeError;

use crate::namespace_path::{LookupPath, NamespacePath};
use crate::{
    GroupId, NamespaceId, ProjectId, RepoId, RepositoryKind, RepositoryState, Role, TombstoneId,
    UserId, Visibility,
};

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

/// What forge-auth's project view needs for one viewer and one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectAccess {
    pub visibility: Visibility,
    /// The viewer's native effective role (direct, ancestor groups, owner
    /// of the personal namespace); `None` = no role or no viewer.
    pub role: Option<Role>,
    /// Deleted and inside the grace period (`deleted_at` set, not purged).
    pub pending_deletion: bool,
}

/// A project's repository as the resolver read it (M1 phase 9 MR3): the
/// storage id (ADR-0009) and whether the bare directory exists yet. Read by
/// the same statement as the access facts; a Smart HTTP route uses it only
/// after `authorize()` allowed (`FG-REPO-0016` while `provisioning`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectRepository {
    pub id: RepoId,
    pub state: RepositoryState,
}

/// A live namespace with everything authorization needs about it for the
/// viewer of the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedTarget {
    /// A personal namespace (ownership is `Resource::Namespace { owner }`).
    User(UserId),
    /// A group and the viewer's effective role on it (`None` = no role or
    /// no viewer).
    Group {
        id: GroupId,
        viewer_role: Option<Role>,
    },
    /// A project, the viewer's access facts and its repository (one
    /// statement; the repository is not an authorization input).
    Project {
        id: ProjectId,
        access: ProjectAccess,
        repository: ProjectRepository,
    },
}

/// A live namespace a path leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// The `forge.namespaces` row (logged as `namespace_id`).
    pub namespace_id: NamespaceId,
    pub target: ResolvedTarget,
}

impl Resolved {
    pub fn owner(&self) -> NamespaceOwner {
        match self.target {
            ResolvedTarget::User(id) => NamespaceOwner::User(id),
            ResolvedTarget::Group { id, .. } => NamespaceOwner::Group(id),
            ResolvedTarget::Project { id, .. } => NamespaceOwner::Project(id),
        }
    }

    pub fn kind(&self) -> NamespaceKind {
        self.owner().kind()
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
    /// `canonical_path`). The caller must still authorize before revealing
    /// it.
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

/// Path → namespace + the viewer's authorization inputs, for the
/// dispatcher (steps 5–6) and `GET /api/v1/namespaces/resolve`.
pub trait NamespaceResolver: Send + Sync {
    /// Exact matches only (see [`Resolution`]); `viewer` is the
    /// authenticated user, `None` for an anonymous caller (the same path:
    /// a public shell never calls this, a clone may).
    ///
    /// **Contract**: an implementation MUST issue the same statements, in
    /// the same number, whatever the outcome (live, moved, deleted, absent;
    /// user, group or project) and whether or not the viewer may see the
    /// target — forge-db does it in one statement. Visibility is decided by
    /// `authorize()`, never here. Absence is [`Resolution::Absent`], not an
    /// error; errors are store failures (`FG-DB-…`) or rows that cannot be
    /// read. The caller bounds the call with its own timeout.
    /// (`prefix=true` resolution arrives with the M2 rescue of
    /// GitHub-shaped URLs.)
    fn resolve<'a>(
        &'a self,
        path: &'a LookupPath,
        viewer: Option<UserId>,
    ) -> LookupFuture<'a, Resolution>;
}

/// One project by id, as one row: the viewer's access facts (decided by
/// `authorize()`) and what `GET /api/v1/projects/{id}` shows once it is
/// allowed (M1 phase 8 MR5). Everything comes from the same statement, so
/// the id, the facts and the details cannot belong to different projects;
/// the caller reveals nothing of it before `authorize()` allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRecord {
    pub id: ProjectId,
    pub access: ProjectAccess,
    /// The project's `forge.namespaces` row.
    pub namespace_id: NamespaceId,
    /// The project's current path (`forge.namespaces.full_path`, confirmed
    /// against its segments).
    pub path: NamespacePath,
    pub display_name: String,
    pub description: String,
    /// The kind of the project's repository (ADR-0004).
    pub kind: RepositoryKind,
    /// `projects.created_at` in UTC, RFC 3339 (`2026-10-31T09:00:00Z`).
    pub created_at: String,
}

/// Projects by id, for routes that address a project by id
/// (`GET /api/v1/projects/{id}`, the permalink's API).
pub trait ProjectAccessLoader: Send + Sync {
    /// `None` when the project does not exist. `viewer` as in
    /// [`NamespaceResolver::resolve`], with the same contract: the same
    /// statements whether the project exists, is visible or not (forge-db:
    /// one statement). The caller bounds the call with its own timeout.
    fn load_project<'a>(
        &'a self,
        project: ProjectId,
        viewer: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>>;
}
