//! The contexts of the group and project write routes (M1 phase 8b MR7a):
//! a group ([`GroupCtx`]), a personal namespace ([`UserNamespaceCtx`]),
//! the parent a project is created under ([`ParentCtx`]) and a visibility
//! change ([`VisibilityChangeCtx`]); MR8 adds one membership change of a
//! project ([`ProjectMemberCtx`]) or a group ([`GroupMemberCtx`]) and one
//! protection rule change ([`RefProtectionCtx`]). Sealed like every [`ResourceCtx`]:
//! private fields, one constructor taking one loaded row, so the id a proof
//! carries and the facts it was decided on cannot come from different rows.
//!
//! ```compile_fail
//! use forge_auth::{GroupCtx, LoadedGroup};
//! use forge_core::GroupId;
//! let mixed = GroupCtx { group: LoadedGroup { id: GroupId::new(1), role: None } };
//! ```

use forge_core::{GroupId, NamespaceId, ProjectId, ProtectedRefId, UserId};

use crate::model::{MembershipChange, MembershipScope, ProjectView, Resource, Role, Visibility};
use crate::prove::{sealed, LoadedProject, ResourceCtx};

/// One group as loaded for the caller (one row: id and role together).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedGroup {
    pub id: GroupId,
    /// The caller's effective role (direct or from an ancestor group;
    /// `None` = no role).
    pub role: Option<Role>,
}

/// A group (`GroupRead`, `GroupUpdate`, `SubgroupCreate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupCtx {
    group: LoadedGroup,
}

impl GroupCtx {
    pub fn new(group: LoadedGroup) -> Self {
        Self { group }
    }

    /// The caller's role `authorize()` decides on.
    pub fn role(&self) -> Option<Role> {
        self.group.role
    }
}

impl sealed::Sealed for GroupCtx {}

impl ResourceCtx for GroupCtx {
    type Id = GroupId;
    fn resource(&self) -> Resource {
        Resource::Group {
            role: self.group.role,
        }
    }
    fn id(&self) -> GroupId {
        self.group.id
    }
}

/// A personal namespace as loaded (its row and its owner).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedUserNamespace {
    pub namespace: NamespaceId,
    pub owner: UserId,
}

/// A personal namespace (`ProjectCreate` in it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserNamespaceCtx {
    namespace: LoadedUserNamespace,
}

impl UserNamespaceCtx {
    pub fn new(namespace: LoadedUserNamespace) -> Self {
        Self { namespace }
    }

    pub fn owner(&self) -> UserId {
        self.namespace.owner
    }
}

impl sealed::Sealed for UserNamespaceCtx {}

impl ResourceCtx for UserNamespaceCtx {
    type Id = NamespaceId;
    fn resource(&self) -> Resource {
        Resource::Namespace {
            owner: self.namespace.owner,
        }
    }
    fn id(&self) -> NamespaceId {
        self.namespace.namespace
    }
}

/// The namespace a project is created under, as loaded: a group (with the
/// caller's role on it) or a personal namespace (with its owner). A
/// project namespace is never a parent (the loader answers it as absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadedParent {
    Group {
        namespace: NamespaceId,
        group: LoadedGroup,
    },
    User(LoadedUserNamespace),
}

/// The parent of a new project (`ProjectCreate`). Its id is the parent's
/// namespace id; the resource is the group's or the personal namespace's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentCtx {
    parent: LoadedParent,
}

impl ParentCtx {
    pub fn new(parent: LoadedParent) -> Self {
        Self { parent }
    }

    pub fn parent(&self) -> LoadedParent {
        self.parent
    }
}

impl sealed::Sealed for ParentCtx {}

impl ResourceCtx for ParentCtx {
    type Id = NamespaceId;
    fn resource(&self) -> Resource {
        match self.parent {
            LoadedParent::Group { group, .. } => Resource::Group { role: group.role },
            LoadedParent::User(ns) => Resource::Namespace { owner: ns.owner },
        }
    }
    fn id(&self) -> NamespaceId {
        match self.parent {
            LoadedParent::Group { namespace, .. } => namespace,
            LoadedParent::User(ns) => ns.namespace,
        }
    }
}

/// A project's visibility about to change to `to` (`ProjectVisibilityChange`):
/// the project as loaded and the requested visibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityChangeCtx {
    project: LoadedProject,
    to: Visibility,
}

impl VisibilityChangeCtx {
    pub fn new(project: LoadedProject, to: Visibility) -> Self {
        Self { project, to }
    }

    /// The requested visibility.
    pub fn to(&self) -> Visibility {
        self.to
    }

    /// The project's view `authorize()` decides on.
    pub fn view(&self) -> ProjectView {
        ProjectView {
            visibility: self.project.visibility,
            role: self.project.role,
            pending_deletion: self.project.pending_deletion,
        }
    }
}

impl sealed::Sealed for VisibilityChangeCtx {}

impl ResourceCtx for VisibilityChangeCtx {
    type Id = forge_core::ProjectId;
    fn resource(&self) -> Resource {
        Resource::VisibilityChange {
            project: self.view(),
            to: self.to,
        }
    }
    fn id(&self) -> forge_core::ProjectId {
        self.project.id
    }
}

/// One membership change as loaded (M1 phase 8b MR8): the member, what is
/// done to the membership — `from` is the member's **direct** role read
/// from the database (inside the write transaction for the decision that
/// binds the write), never from the request — and whether it would leave
/// the project or group without an Owner (computed where the members are
/// read, never by the client).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedMembership {
    pub member: UserId,
    pub change: MembershipChange,
    pub removes_last_owner: bool,
}

fn project_view(project: &LoadedProject) -> ProjectView {
    ProjectView {
        visibility: project.visibility,
        role: project.role,
        pending_deletion: project.pending_deletion,
    }
}

/// One membership change of a project (`MemberManage`). Its id is the
/// project and the member together, so a proof for one member cannot act
/// on another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMemberCtx {
    project: LoadedProject,
    membership: LoadedMembership,
}

impl ProjectMemberCtx {
    pub fn new(project: LoadedProject, membership: LoadedMembership) -> Self {
        Self {
            project,
            membership,
        }
    }

    pub fn membership(&self) -> LoadedMembership {
        self.membership
    }
}

impl sealed::Sealed for ProjectMemberCtx {}

impl ResourceCtx for ProjectMemberCtx {
    type Id = (ProjectId, UserId);
    fn resource(&self) -> Resource {
        Resource::Membership {
            scope: MembershipScope::Project(project_view(&self.project)),
            change: self.membership.change,
            removes_last_owner: self.membership.removes_last_owner,
        }
    }
    fn id(&self) -> (ProjectId, UserId) {
        (self.project.id, self.membership.member)
    }
}

/// One membership change of a group (`MemberManage`); the id is the group
/// and the member together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMemberCtx {
    group: LoadedGroup,
    membership: LoadedMembership,
}

impl GroupMemberCtx {
    pub fn new(group: LoadedGroup, membership: LoadedMembership) -> Self {
        Self { group, membership }
    }

    pub fn membership(&self) -> LoadedMembership {
        self.membership
    }
}

impl sealed::Sealed for GroupMemberCtx {}

impl ResourceCtx for GroupMemberCtx {
    type Id = (GroupId, UserId);
    fn resource(&self) -> Resource {
        Resource::Membership {
            scope: MembershipScope::Group {
                role: self.group.role,
            },
            change: self.membership.change,
            removes_last_owner: self.membership.removes_last_owner,
        }
    }
    fn id(&self) -> (GroupId, UserId) {
        (self.group.id, self.membership.member)
    }
}

/// One protection rule change as loaded (M1 phase 8b MR8): the rule
/// (`None` for a new one), the minimum push role it has now (`from`; `None`
/// for a new rule or one the project does not have) and the one requested
/// (`to`; `None` for a delete). `from` is read from the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedProtection {
    pub rule: Option<ProtectedRefId>,
    pub from: Option<Role>,
    pub to: Option<Role>,
}

/// A protection rule change on a project (`ProtectedRefManage`); the id is
/// the project and the rule together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefProtectionCtx {
    project: LoadedProject,
    protection: LoadedProtection,
}

impl RefProtectionCtx {
    pub fn new(project: LoadedProject, protection: LoadedProtection) -> Self {
        Self {
            project,
            protection,
        }
    }

    pub fn protection(&self) -> LoadedProtection {
        self.protection
    }
}

impl sealed::Sealed for RefProtectionCtx {}

impl ResourceCtx for RefProtectionCtx {
    type Id = (ProjectId, Option<ProtectedRefId>);
    fn resource(&self) -> Resource {
        Resource::RefProtectionChange {
            project: project_view(&self.project),
            from: self.protection.from,
            to: self.protection.to,
        }
    }
    fn id(&self) -> (ProjectId, Option<ProtectedRefId>) {
        (self.project.id, self.protection.rule)
    }
}
