//! `prove()` and `Authorized<R>` — the proof that `authorize()` allowed one
//! action on one resource (ADR-0015 "認可の証明（`Authorized<R>`）").
//!
//! `prove()` holds no rule of its own: it calls [`authorize`] and, on `Ok`,
//! wraps the typed context it decided on. A handler takes the proof and reads
//! the target id from it, never from the path again; the route registry checks
//! `proof.action() == def.action` before it calls the handler.
//!
//! The proof cannot be forged: its fields are private and it has no public
//! constructor, so a struct literal outside this crate does not compile:
//!
//! ```compile_fail
//! use forge_auth::{Action, Authorized, InstanceCtx};
//! let forged = Authorized::<InstanceCtx> { action: Action::Metrics, id: (), ctx: InstanceCtx };
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
//! The same code without the forbidden line compiles, so the three tests
//! above fail for the reason they name:
//!
//! ```
//! use forge_auth::{prove, Action, InstanceCtx, Principal};
//! let Ok(proof) = prove(&Principal::anonymous(), Action::Health, InstanceCtx) else {
//!     panic!("health is public");
//! };
//! assert_eq!(proof.action(), Action::Health);
//! assert_eq!(proof.id(), &());
//! ```

use std::fmt;

use forge_core::{NamespaceId, ProjectId};
use forge_errors::ForgeError;

use crate::action::Action;
use crate::authorize::authorize;
use crate::model::{
    NamespaceView, Principal, ProjectView, ResolvedVia, Resource, UnresolvedReason,
};

/// A typed resource context: what `authorize()` decides on (`resource()`)
/// and the id of the target the handler acts on (`id()`). Built by the
/// lookup layer from the database, never from the request.
pub trait ResourceCtx {
    type Id;
    /// The resource `authorize()` decides on.
    fn resource(&self) -> Resource;
    /// The target's id, carried by the proof.
    fn id(&self) -> Self::Id;
}

/// Proof that `authorize()` allowed `action` on `ctx`. Only [`prove`] makes
/// one; it is neither `Clone` nor `Default` and has no public constructor.
#[must_use = "a proof is the only way to reach the handler's target"]
pub struct Authorized<R: ResourceCtx> {
    action: Action,
    id: R::Id,
    ctx: R,
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
    })
}

/// The instance itself (`Health`, `WebShell`, user administration, metrics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceCtx;

impl ResourceCtx for InstanceCtx {
    type Id = ();
    fn resource(&self) -> Resource {
        Resource::Instance
    }
    fn id(&self) {}
}

/// A project as seen by the caller (`ProjectRead`, `RepoRead`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectCtx {
    pub id: ProjectId,
    pub view: ProjectView,
}

impl ResourceCtx for ProjectCtx {
    type Id = ProjectId;
    fn resource(&self) -> Resource {
        Resource::Project(self.view)
    }
    fn id(&self) -> ProjectId {
        self.id
    }
}

/// A path that resolved to a namespace (`PathResolve`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedCtx {
    pub namespace: NamespaceId,
    pub via: ResolvedVia,
    pub view: NamespaceView,
}

impl ResourceCtx for ResolvedCtx {
    type Id = NamespaceId;
    fn resource(&self) -> Resource {
        Resource::Resolved {
            via: self.via,
            view: self.view,
        }
    }
    fn id(&self) -> NamespaceId {
        self.namespace
    }
}

/// A path that resolved to nothing (`PathResolve`). `prove()` on it always
/// fails; it exists so the dispatcher decides every resolution through the
/// same call (`FG-AUTH-0001` / `FG-AUTH-0004` / `FG-REPO-0005`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnresolvedCtx(pub UnresolvedReason);

impl ResourceCtx for UnresolvedCtx {
    type Id = ();
    fn resource(&self) -> Resource {
        Resource::Unresolved(self.0)
    }
    fn id(&self) {}
}
