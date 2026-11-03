//! The integration feed's context and its visible set (M3 I1c, ADR-0016 §7,
//! plan 02 §11.2).
//!
//! - [`IntegrationFeedCtx`]: what the feed routes decide on — the
//!   integration of the path as loaded (its service account, whether it is
//!   enabled), whether the caller is that service account, and whether the
//!   request carried `Forge-Act-As`. Sealed like every context.
//! - [`prove_visible`]: on the route's proof and the material forge-db read
//!   in the feed's snapshot, decide the integration again on the fresh
//!   facts, then every bound project × [`VisibilityClass`] with
//!   `authorize()` (`Resource::ProjectEvents`), and build the
//!   [`VisibleSet`] of what was allowed for the integration class, narrowed
//!   by the integration's allowlist. The one caller of
//!   `VisibleSet::from_authorized_decisions` (gate:
//!   forge-integ `tests/decided_callers.rs`). At most 200 bindings × 4
//!   classes: pure calls, no I/O.

use forge_core::{
    FeedMaterial, IntegrationId, UserId, VisibilityClass, VisibleProject, VisibleSet,
};
use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::authorize::authorize;
use crate::model::{IntegrationFeedView, ProjectView, Resource};
use crate::prove::{sealed, Authorized, ResourceCtx};

/// An integration as the feed route loaded it: the service account it acts
/// as and whether it is enabled (one row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedIntegrationFeed {
    pub integration: IntegrationId,
    pub service_account: UserId,
    pub enabled: bool,
}

/// The feed routes' context (`IntegrationEventRead`,
/// `IntegrationEventCursorRead`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationFeedCtx {
    loaded: LoadedIntegrationFeed,
    caller: Option<UserId>,
    act_as: bool,
}

impl IntegrationFeedCtx {
    /// `caller`: the authenticated user the route runs for; `act_as`: the
    /// request carried a `Forge-Act-As` header (whatever its value).
    pub fn new(loaded: LoadedIntegrationFeed, caller: Option<UserId>, act_as: bool) -> Self {
        Self {
            loaded,
            caller,
            act_as,
        }
    }

    fn view(&self) -> IntegrationFeedView {
        IntegrationFeedView {
            own: self.caller == Some(self.loaded.service_account),
            enabled: self.loaded.enabled,
            act_as: self.act_as,
        }
    }
}

impl sealed::Sealed for IntegrationFeedCtx {}

impl ResourceCtx for IntegrationFeedCtx {
    type Id = IntegrationId;
    fn resource(&self) -> Resource {
        Resource::IntegrationFeed(self.view())
    }
    fn id(&self) -> IntegrationId {
        self.loaded.integration
    }
}

/// `FG-SYS-0001`: the material is not the proof's integration (a caller
/// bug, never a pass).
fn mismatch(reason: &'static str, proof: &Authorized<IntegrationFeedCtx>) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("integration_id", proof.id().get())
}

/// What the feed may show in this request (see the module docs). Denies
/// like `authorize()` when the fresh facts refuse the integration (it was
/// disabled since the route decided: `FG-AUTH-0003
/// reason=integration_disabled`); a project or class `authorize()` refuses
/// is simply not in the set.
pub fn prove_visible(
    proof: &Authorized<IntegrationFeedCtx>,
    material: &FeedMaterial,
) -> Result<VisibleSet, ForgeError> {
    if material.integration != *proof.id() {
        return Err(mismatch("feed_material_other_integration", proof));
    }
    let principal = proof.principal();
    let fresh = IntegrationFeedCtx::new(
        LoadedIntegrationFeed {
            integration: material.integration,
            service_account: material.service_account,
            enabled: material.enabled,
        },
        principal.user(),
        proof.ctx().act_as,
    );
    authorize(principal, proof.action(), &fresh.resource())?;
    let mut allowed = Vec::new();
    for binding in &material.bindings {
        let Some(access) = binding.access else {
            continue;
        };
        let project = ProjectView {
            visibility: access.visibility,
            role: access.role,
            pending_deletion: access.pending_deletion,
        };
        for class in VisibilityClass::ALL {
            let decided = authorize(
                principal,
                Action::IntegrationEventRead,
                &Resource::ProjectEvents { project, class },
            );
            match (decided, class) {
                (Ok(()), VisibilityClass::Integration) => allowed.push(VisibleProject {
                    project: binding.project,
                    generation: binding.generation,
                }),
                // Never the feed's: an allow here is a bug in authorize(),
                // refused rather than shown (fail closed).
                (
                    Ok(()),
                    VisibilityClass::Maintainer
                    | VisibilityClass::Deployment
                    | VisibilityClass::Admin,
                ) => {
                    return Err(
                        mismatch("feed_class_allowed", proof).with_context("class", class.as_str())
                    )
                }
                (Err(_), _) => {}
            }
        }
    }
    Ok(VisibleSet::from_authorized_decisions(
        material.integration,
        allowed,
        &material.event_types,
    ))
}
