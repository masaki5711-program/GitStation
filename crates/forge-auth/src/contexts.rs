//! The contexts of the group and project write routes (M1 phase 8b MR7a):
//! a group ([`GroupCtx`]), a personal namespace ([`UserNamespaceCtx`]),
//! the parent a project is created under ([`ParentCtx`]) and a visibility
//! change ([`VisibilityChangeCtx`]); MR8 adds one membership change of a
//! project ([`ProjectMemberCtx`]) or a group ([`GroupMemberCtx`]) and one
//! protection rule change ([`RefProtectionCtx`]); D5 adds a project move
//! ([`TransferCtx`]), a group move ([`GroupTransferCtx`]), a user rename
//! ([`UserRenameCtx`]) and a tombstone release ([`TombstoneCtx`]). Sealed
//! like every [`ResourceCtx`]:
//! private fields, one constructor taking one loaded row, so the id a proof
//! carries and the facts it was decided on cannot come from different rows.
//!
//! ```compile_fail
//! use forge_auth::{GroupCtx, LoadedGroup};
//! use forge_core::GroupId;
//! let mixed = GroupCtx { group: LoadedGroup { id: GroupId::new(1), role: None } };
//! ```

use forge_core::{GroupId, NamespaceId, ProjectId, ProtectedRefId, TombstoneId, UserId};

use crate::model::{
    MembershipChange, MembershipScope, ProjectView, Resource, Role, Scope, SessionRoute,
    TombstoneRelease, TransferTarget, Visibility,
};
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

/// Where a move goes, as loaded for the caller (D5): a group with the
/// caller's role on it, a personal namespace with its user, the top level,
/// or a target the caller cannot see — which the loader also answers for a
/// target that does not exist, so both are the same `FG-REPO-0005`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadedTransferTarget {
    Group {
        namespace: NamespaceId,
        group: LoadedGroup,
    },
    User(LoadedUserNamespace),
    TopLevel,
    NotVisible,
    /// The parent the resource has now (`None` = the top level): a rename,
    /// decided by `authorize()` (`TransferTarget::CurrentParent`).
    CurrentParent {
        namespace: Option<NamespaceId>,
    },
}

impl LoadedTransferTarget {
    /// The target `authorize()` decides on.
    pub fn target(&self) -> TransferTarget {
        match *self {
            LoadedTransferTarget::Group { group, .. } => TransferTarget::Group {
                role: group.role,
                visible: true,
            },
            LoadedTransferTarget::User(ns) => TransferTarget::PersonalNamespace { owner: ns.owner },
            LoadedTransferTarget::TopLevel => TransferTarget::TopLevel,
            LoadedTransferTarget::NotVisible => TransferTarget::Group {
                role: None,
                visible: false,
            },
            LoadedTransferTarget::CurrentParent { namespace } => TransferTarget::CurrentParent {
                top_level: namespace.is_none(),
            },
        }
    }

    /// The target parent's namespace (`None` = the top level; a target
    /// the caller cannot see never reaches a proof).
    pub fn namespace(&self) -> Option<NamespaceId> {
        match *self {
            LoadedTransferTarget::Group { namespace, .. } => Some(namespace),
            LoadedTransferTarget::User(ns) => Some(ns.namespace),
            LoadedTransferTarget::CurrentParent { namespace } => namespace,
            LoadedTransferTarget::TopLevel | LoadedTransferTarget::NotVisible => None,
        }
    }
}

/// A project moving (or being renamed) to `target` (`ProjectTransfer`).
/// The id is the project and the target parent together, so a proof for
/// one destination cannot move the project elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferCtx {
    project: LoadedProject,
    target: LoadedTransferTarget,
}

impl TransferCtx {
    pub fn new(project: LoadedProject, target: LoadedTransferTarget) -> Self {
        Self { project, target }
    }

    pub fn target(&self) -> LoadedTransferTarget {
        self.target
    }
}

impl sealed::Sealed for TransferCtx {}

impl ResourceCtx for TransferCtx {
    type Id = (ProjectId, Option<NamespaceId>);
    fn resource(&self) -> Resource {
        Resource::Transfer {
            source: project_view(&self.project),
            target: self.target.target(),
        }
    }
    fn id(&self) -> (ProjectId, Option<NamespaceId>) {
        (self.project.id, self.target.namespace())
    }
}

/// A group moving (or being renamed) to `target` (`GroupTransfer`); the id
/// is the group and the target parent together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupTransferCtx {
    group: LoadedGroup,
    target: LoadedTransferTarget,
}

impl GroupTransferCtx {
    pub fn new(group: LoadedGroup, target: LoadedTransferTarget) -> Self {
        Self { group, target }
    }

    pub fn target(&self) -> LoadedTransferTarget {
        self.target
    }
}

impl sealed::Sealed for GroupTransferCtx {}

impl ResourceCtx for GroupTransferCtx {
    type Id = (GroupId, Option<NamespaceId>);
    fn resource(&self) -> Resource {
        Resource::GroupTransfer {
            source_role: self.group.role,
            target: self.target.target(),
        }
    }
    fn id(&self) -> (GroupId, Option<NamespaceId>) {
        (self.group.id, self.target.namespace())
    }
}

/// The user whose username and personal namespace path change
/// (`UserRename`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRenameCtx {
    target_user: UserId,
}

impl UserRenameCtx {
    pub fn new(target_user: UserId) -> Self {
        Self { target_user }
    }
}

impl sealed::Sealed for UserRenameCtx {}

impl ResourceCtx for UserRenameCtx {
    type Id = UserId;
    fn resource(&self) -> Resource {
        Resource::UserRename {
            target_user: self.target_user,
        }
    }
    fn id(&self) -> UserId {
        self.target_user
    }
}

/// One tombstone as loaded for the caller (one row: its id and the facts
/// its release is decided on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedTombstone {
    pub id: TombstoneId,
    pub release: TombstoneRelease,
}

/// A tombstone about to be released (`NamespaceTombstoneRelease`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TombstoneCtx {
    tombstone: LoadedTombstone,
}

impl TombstoneCtx {
    pub fn new(tombstone: LoadedTombstone) -> Self {
        Self { tombstone }
    }

    pub fn release(&self) -> TombstoneRelease {
        self.tombstone.release
    }
}

impl sealed::Sealed for TombstoneCtx {}

impl ResourceCtx for TombstoneCtx {
    type Id = TombstoneId;
    fn resource(&self) -> Resource {
        Resource::Tombstone(self.tombstone.release)
    }
    fn id(&self) -> TombstoneId {
        self.tombstone.id
    }
}

// ── M2 MR6: tokens, sessions and user administration ──

/// One personal access token as loaded (its id and owner from one row):
/// `TokenRevoke`. The id is the token's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenCtx {
    token_id: i64,
    owner: UserId,
}

impl TokenCtx {
    pub fn new(token_id: i64, owner: UserId) -> Self {
        Self { token_id, owner }
    }

    pub fn owner(&self) -> UserId {
        self.owner
    }
}

impl sealed::Sealed for TokenCtx {}

impl ResourceCtx for TokenCtx {
    type Id = i64;
    fn resource(&self) -> Resource {
        Resource::Token { owner: self.owner }
    }
    fn id(&self) -> i64 {
        self.token_id
    }
}

/// The tokens of `owner` as a whole (`TokenList`): the id is the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenOwnerCtx {
    owner: UserId,
}

impl TokenOwnerCtx {
    pub fn new(owner: UserId) -> Self {
        Self { owner }
    }
}

impl sealed::Sealed for TokenOwnerCtx {}

impl ResourceCtx for TokenOwnerCtx {
    type Id = UserId;
    fn resource(&self) -> Resource {
        Resource::Token { owner: self.owner }
    }
    fn id(&self) -> UserId {
        self.owner
    }
}

/// A token about to be created for `owner` with `scopes` (`TokenCreate`):
/// the id is the owner, so the proof names whose token it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRequestCtx {
    owner: UserId,
    scopes: Vec<Scope>,
}

impl TokenRequestCtx {
    pub fn new(owner: UserId, scopes: Vec<Scope>) -> Self {
        Self { owner, scopes }
    }

    /// The scopes that were decided on.
    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }
}

impl sealed::Sealed for TokenRequestCtx {}

impl ResourceCtx for TokenRequestCtx {
    type Id = UserId;
    fn resource(&self) -> Resource {
        Resource::TokenRequest {
            owner: self.owner,
            scopes: self.scopes.clone(), // clone: the context keeps its scopes
        }
    }
    fn id(&self) -> UserId {
        self.owner
    }
}

/// The web sessions of `owner` through `route` (`SessionList`,
/// `SessionRevoke`; ADR-0021 §17). `session` is the one session a route
/// acts on (`None`: all of the owner's). The id is the owner and the
/// session together, so a proof for one session cannot revoke another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCtx {
    owner: UserId,
    session: Option<i64>,
    route: SessionRoute,
}

impl SessionCtx {
    pub fn new(owner: UserId, session: Option<i64>, route: SessionRoute) -> Self {
        Self {
            owner,
            session,
            route,
        }
    }

    /// The route the sessions were reached through.
    pub fn route(&self) -> SessionRoute {
        self.route
    }
}

impl sealed::Sealed for SessionCtx {}

impl ResourceCtx for SessionCtx {
    type Id = (UserId, Option<i64>);
    fn resource(&self) -> Resource {
        Resource::Session {
            owner: self.owner,
            route: self.route,
        }
    }
    fn id(&self) -> (UserId, Option<i64>) {
        (self.owner, self.session)
    }
}

/// A user an administrator acts on (`UserDisable`): decided on the
/// instance (administrator rights), the proof carries the target user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTargetCtx {
    target: UserId,
}

impl UserTargetCtx {
    pub fn new(target: UserId) -> Self {
        Self { target }
    }
}

impl sealed::Sealed for UserTargetCtx {}

impl ResourceCtx for UserTargetCtx {
    type Id = UserId;
    fn resource(&self) -> Resource {
        Resource::Instance
    }
    fn id(&self) -> UserId {
        self.target
    }
}
