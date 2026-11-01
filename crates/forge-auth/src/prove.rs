//! `prove()` and `Authorized<R>` — the proof that `authorize()` allowed one
//! action on one resource (ADR-0015 "認可の証明（`Authorized<R>`）").
//!
//! `prove()` holds no rule of its own: it calls [`authorize`] and, on `Ok`,
//! wraps the typed context it decided on. A handler takes the proof and reads
//! the target id from it, never from the path again; the route registry checks
//! `proof.action() == def.action` before it calls the handler (MR4).
//!
//! The proof cannot be forged: its fields are private and it has no public
//! constructor, so a struct literal outside this crate does not compile:
//!
//! ```compile_fail
//! use forge_auth::{Action, Authorized, InstanceCtx, Principal};
//! let forged = Authorized::<InstanceCtx> {
//!     action: Action::Metrics, id: (), ctx: InstanceCtx, principal: Principal::anonymous(),
//! };
//! ```
//!
//! nor can it be defaulted:
//!
//! ```compile_fail
//! use forge_auth::{Authorized, InstanceCtx};
//! let forged: Authorized<InstanceCtx> = Default::default();
//! ```
//!
//! nor cloned (one decision, one proof):
//!
//! ```compile_fail
//! use forge_auth::{prove, Action, InstanceCtx, Principal};
//! fn needs_clone<T: Clone>(_: &T) {}
//! let Ok(proof) = prove(&Principal::anonymous(), Action::Health, InstanceCtx) else { return };
//! needs_clone(&proof);
//! ```
//!
//! The id a proof carries comes from the same value the decision reads: a
//! context has private fields and one constructor that takes one loaded row
//! (id and facts together), so a project's id cannot be paired with another
//! project's view by a struct literal:
//!
//! ```compile_fail
//! use forge_auth::{LoadedProject, ProjectCtx, Visibility};
//! use forge_core::ProjectId;
//! let project = LoadedProject {
//!     id: ProjectId::new(2), visibility: Visibility::Internal, role: None, pending_deletion: false,
//! };
//! let mixed = ProjectCtx { project };
//! ```
//!
//! and `ResourceCtx` is sealed — no other crate can implement a context
//! whose `id()` disagrees with its `resource()`:
//!
//! ```compile_fail
//! use forge_auth::{Resource, ResourceCtx};
//! struct Mine;
//! impl ResourceCtx for Mine {
//!     type Id = ();
//!     fn resource(&self) -> Resource { Resource::Instance }
//!     fn id(&self) {}
//! }
//! ```
//!
//! The same code without the forbidden parts compiles, so the tests above
//! fail for the reason they name:
//!
//! ```
//! use forge_auth::{
//!     prove, Action, Credential, InstanceCtx, LoadedProject, Principal, ProjectCtx, Role,
//!     Visibility,
//! };
//! use forge_core::{ProjectId, UserId};
//! let Ok(proof) = prove(&Principal::anonymous(), Action::Health, InstanceCtx) else {
//!     panic!("health is public");
//! };
//! assert_eq!(proof.action(), Action::Health);
//! assert_eq!(proof.id(), &());
//! let ctx = ProjectCtx::new(LoadedProject {
//!     id: ProjectId::new(2),
//!     visibility: Visibility::Private,
//!     role: Some(Role::Reporter),
//!     pending_deletion: false,
//! });
//! let me = Principal::authenticated(UserId::new(1), false, true, Credential::Session);
//! let Ok(proof) = prove(&me, Action::ProjectRead, ctx) else { panic!("reporter reads") };
//! assert_eq!(proof.id(), &ProjectId::new(2));
//! ```

use std::fmt;

use forge_core::{GroupId, NamespaceId, NamespaceOwner, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::authorize::authorize;
use crate::model::{
    NamespaceView, Principal, ProjectView, ResolvedVia, Resource, Role, UnresolvedReason,
    Visibility,
};

pub use crate::contexts::{
    GroupCtx, GroupMemberCtx, LoadedGroup, LoadedMembership, LoadedParent, LoadedProtection,
    LoadedUserNamespace, ParentCtx, ProjectMemberCtx, RefProtectionCtx, UserNamespaceCtx,
    VisibilityChangeCtx,
};

/// Seals [`ResourceCtx`]: only this crate implements it, so every context's
/// `id()` and `resource()` read the same private value.
pub(crate) mod sealed {
    pub trait Sealed {}
}

/// A typed resource context: what `authorize()` decides on (`resource()`)
/// and the id of the target the handler acts on (`id()`). Sealed; built by
/// the lookup layer from one loaded row, never from the request.
pub trait ResourceCtx: sealed::Sealed {
    type Id;
    /// The resource `authorize()` decides on.
    fn resource(&self) -> Resource;
    /// The target's id, carried by the proof.
    fn id(&self) -> Self::Id;
}

/// Proof that `authorize()` allowed `action` on `ctx`. Only [`prove`] makes
/// one; it is neither `Clone` nor `Default` and has no public constructor.
///
/// The action is a runtime value: a proof for `WebShell` has the same type
/// as one for `UserCreate`. The route registry must compare `action()` with
/// the route's action before every handler (ADR-0015, MR4).
#[must_use = "a proof is the only way to reach the handler's target"]
pub struct Authorized<R: ResourceCtx> {
    action: Action,
    id: R::Id,
    ctx: R,
    /// Who was decided on: decisions made later on this proof (the refs of
    /// a push, `authorize_ref_updates`) use this principal, never one the
    /// caller passes, so a proof cannot be paired with another caller.
    principal: Principal,
}

impl<R: ResourceCtx> Authorized<R> {
    /// The action that was decided.
    pub fn action(&self) -> Action {
        self.action
    }

    /// The id of the target that was decided.
    pub fn id(&self) -> &R::Id {
        &self.id
    }

    /// The context that was decided.
    pub fn ctx(&self) -> &R {
        &self.ctx
    }

    /// The principal the decision was made for (read-only: a command acts
    /// for this caller — forge-hosting builds its change context and loads
    /// the caller's facts from it — and decides again with
    /// [`Authorized::reconfirm`], never with another principal).
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Decide the same action again, for the same caller, on `fresh` — the
    /// same target re-loaded inside the write transaction that will act on
    /// it (M1 phase 8b MR7a: no time-of-check / time-of-use gap). The
    /// caller's account facts (`account`) are re-read there too: the
    /// principal keeps its user and credential (a token keeps its scopes),
    /// with the fresh `active` and administrator flags. Denies exactly like
    /// [`prove`]; a `fresh` context for another target than this proof's is
    /// a programming error (`FG-SYS-0001 reason=reconfirm_target_mismatch`),
    /// never a pass.
    pub fn reconfirm(&self, account: AccountFacts, fresh: R) -> Result<Authorized<R>, ForgeError>
    where
        R::Id: PartialEq,
    {
        if fresh.id() != self.id {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "reconfirm_target_mismatch")
                .with_context("action", self.action.name()));
        }
        let principal = match self.principal.user() {
            Some(user) => Principal::authenticated(
                user,
                account.is_admin,
                account.active,
                self.principal.credential().clone(), // clone: the new proof owns its principal
            )
            .as_agent(self.principal.is_agent()),
            None => Principal::anonymous(),
        };
        prove(&principal, self.action, fresh)
    }
}

/// The caller's own account as re-read inside a write transaction (the
/// user row, locked `FOR SHARE`): [`Authorized::reconfirm`]. A missing row
/// is `active: false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountFacts {
    pub active: bool,
    pub is_admin: bool,
}

impl<R> fmt::Debug for Authorized<R>
where
    R: ResourceCtx + fmt::Debug,
    R::Id: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authorized")
            .field("action", &self.action)
            .field("id", &self.id)
            .field("ctx", &self.ctx)
            .finish()
    }
}

/// Decide with [`authorize`] and, when allowed, return the proof. Denies
/// exactly like `authorize()` (same code, same context): there is one
/// decision point.
pub fn prove<R: ResourceCtx>(
    principal: &Principal,
    action: Action,
    ctx: R,
) -> Result<Authorized<R>, ForgeError> {
    authorize(principal, action, &ctx.resource())?;
    Ok(Authorized {
        action,
        id: ctx.id(),
        ctx,
        principal: principal.clone(), // clone: the proof keeps who it was decided for
    })
}

/// The instance itself (`Health`, `WebShell`, user administration, metrics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceCtx;

impl sealed::Sealed for InstanceCtx {}

impl ResourceCtx for InstanceCtx {
    type Id = ();
    fn resource(&self) -> Resource {
        Resource::Instance
    }
    fn id(&self) {}
}

/// One project as loaded for the caller (one row: id and access facts
/// together). The lookup layer fills it from a single query result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedProject {
    pub id: ProjectId,
    pub visibility: Visibility,
    /// The caller's effective role (`None` = not a member).
    pub role: Option<Role>,
    pub pending_deletion: bool,
}

/// A project as seen by the caller (`ProjectRead`, `RepoRead`, …). The id
/// and the view come from the same [`LoadedProject`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCtx {
    project: LoadedProject,
}

impl ProjectCtx {
    pub fn new(project: LoadedProject) -> Self {
        Self { project }
    }

    /// The view `authorize()` decides on.
    pub fn view(&self) -> ProjectView {
        ProjectView {
            visibility: self.project.visibility,
            role: self.project.role,
            pending_deletion: self.project.pending_deletion,
        }
    }
}

impl sealed::Sealed for ProjectCtx {}

impl ResourceCtx for ProjectCtx {
    type Id = ProjectId;
    fn resource(&self) -> Resource {
        Resource::Project(self.view())
    }
    fn id(&self) -> ProjectId {
        self.project.id
    }
}

/// What a resolved namespace belongs to, with the caller's facts about it:
/// the kind, the owner's id and the facts in one value, so they cannot
/// disagree (built from one resolver row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadedTarget {
    /// A personal namespace and its user.
    User(UserId),
    /// A group and the caller's effective role on it.
    Group { id: GroupId, role: Option<Role> },
    /// A project and the caller's access facts.
    Project(LoadedProject),
}

impl LoadedTarget {
    /// The view `authorize()` decides on.
    pub fn view(&self) -> NamespaceView {
        match *self {
            LoadedTarget::User(_) => NamespaceView::User,
            LoadedTarget::Group { role, .. } => NamespaceView::Group { role },
            LoadedTarget::Project(p) => NamespaceView::Project(ProjectView {
                visibility: p.visibility,
                role: p.role,
                pending_deletion: p.pending_deletion,
            }),
        }
    }

    /// The owner (kind and id).
    pub fn owner(&self) -> NamespaceOwner {
        match *self {
            LoadedTarget::User(id) => NamespaceOwner::User(id),
            LoadedTarget::Group { id, .. } => NamespaceOwner::Group(id),
            LoadedTarget::Project(p) => NamespaceOwner::Project(p.id),
        }
    }
}

/// A path's resolution as loaded for the caller (one row: the namespace id,
/// how the path reached it, and what it belongs to with the caller's facts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedNamespace {
    pub namespace: NamespaceId,
    pub via: ResolvedVia,
    pub target: LoadedTarget,
}

/// A path that resolved to a namespace (`PathResolve`). The id, the owner
/// and the view come from the same [`LoadedNamespace`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCtx {
    resolved: LoadedNamespace,
}

impl ResolvedCtx {
    pub fn new(resolved: LoadedNamespace) -> Self {
        Self { resolved }
    }

    pub fn via(&self) -> ResolvedVia {
        self.resolved.via
    }

    pub fn view(&self) -> NamespaceView {
        self.resolved.target.view()
    }

    /// What the namespace belongs to (the project, group or user id a
    /// handler may reveal once `authorize()` allowed).
    pub fn owner(&self) -> NamespaceOwner {
        self.resolved.target.owner()
    }
}

impl sealed::Sealed for ResolvedCtx {}

impl ResourceCtx for ResolvedCtx {
    type Id = NamespaceId;
    fn resource(&self) -> Resource {
        Resource::Resolved {
            via: self.resolved.via,
            view: self.resolved.target.view(),
        }
    }
    fn id(&self) -> NamespaceId {
        self.resolved.namespace
    }
}

/// A path that resolved to nothing. `prove()` on it fails for every
/// non-public action (`FG-AUTH-0001` / `FG-AUTH-0004`, a wrong-scope token
/// on `PathResolve` `FG-AUTH-0002`, otherwise `FG-REPO-0005`); public
/// actions such as `WebShell` never look a path up. It exists so the
/// dispatcher decides every resolution through the same call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedCtx {
    reason: UnresolvedReason,
}

impl UnresolvedCtx {
    pub fn new(reason: UnresolvedReason) -> Self {
        Self { reason }
    }

    pub fn reason(&self) -> UnresolvedReason {
        self.reason
    }
}

impl sealed::Sealed for UnresolvedCtx {}

impl ResourceCtx for UnresolvedCtx {
    type Id = ();
    fn resource(&self) -> Resource {
        Resource::Unresolved(self.reason)
    }
    fn id(&self) {}
}
