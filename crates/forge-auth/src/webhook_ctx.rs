//! Webhooks (M3 I2, ADR-0016 §8 and its Amendment I2, plan 11 §4.3): the
//! contexts of the routes on one webhook and on one of its deliveries, and
//! [`prove_webhook_delivery`], the decision a delivery is made on.
//!
//! - [`WebhookCtx`] / [`WebhookDeliveryCtx`]: decided on the project as the
//!   loader read it for the caller (`WebhookRead`, `WebhookManage`,
//!   `WebhookDeliveryRead`, `WebhookRedeliver`: Maintainer and above); the
//!   ids are the path's. Whether the webhook belongs to the project, and
//!   the delivery to the webhook, is the command's or read's, after the
//!   decision (`FG-REPO-0005 resource=webhook` / `webhook_delivery`) — an
//!   id of another project is never revealed before it.
//! - [`prove_webhook_delivery`]: on the webhook's material, read when an
//!   event is dispatched and again before each attempt, decide the owner
//!   (the person who created it) as the principal: `WebhookManage` on the
//!   project (still a Maintainer, active, the project not pending
//!   deletion), then every [`VisibilityClass`] with `WebhookDeliver`
//!   (`Resource::ProjectEvents`; the admin class never), and build the
//!   [`WebhookDeliverySet`] of the allowed classes narrowed by the
//!   webhook's allowlist. The owner losing the project is `FG-INT-0010
//!   reason=owner_lost` (with the code `authorize()` answered). The one
//!   caller of `WebhookDeliverySet::from_authorized_decisions` (the
//!   decided-callers gate).

use forge_core::{
    ProjectId, VisibilityClass, WebhookDeliveryId, WebhookDeliverySet, WebhookId, WebhookMaterial,
};
use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::authorize::authorize;
use crate::model::{Credential, Principal, ProjectView, Resource};
use crate::prove::{sealed, LoadedProject, ProjectCtx, ResourceCtx};

/// One webhook of a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookCtx {
    project: ProjectCtx,
    webhook: WebhookId,
}

impl WebhookCtx {
    pub fn new(project: LoadedProject, webhook: WebhookId) -> Self {
        Self {
            project: ProjectCtx::new(project),
            webhook,
        }
    }
}

impl WebhookCtx {
    /// The project as the loader read it for the caller.
    pub fn project_view(&self) -> ProjectView {
        self.project.view()
    }
}

impl sealed::Sealed for WebhookCtx {}

impl ResourceCtx for WebhookCtx {
    type Id = (ProjectId, WebhookId);
    fn resource(&self) -> Resource {
        Resource::Project(self.project.view())
    }
    fn id(&self) -> (ProjectId, WebhookId) {
        (self.project.id(), self.webhook)
    }
}

/// One delivery of a webhook of a project (`WebhookRedeliver`): the pair
/// `(webhook, delivery)` is looked up together, after the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookDeliveryCtx {
    project: ProjectCtx,
    webhook: WebhookId,
    delivery: WebhookDeliveryId,
}

impl WebhookDeliveryCtx {
    pub fn new(project: LoadedProject, webhook: WebhookId, delivery: WebhookDeliveryId) -> Self {
        Self {
            project: ProjectCtx::new(project),
            webhook,
            delivery,
        }
    }
}

impl sealed::Sealed for WebhookDeliveryCtx {}

impl ResourceCtx for WebhookDeliveryCtx {
    type Id = (ProjectId, WebhookId, WebhookDeliveryId);
    fn resource(&self) -> Resource {
        Resource::Project(self.project.view())
    }
    fn id(&self) -> (ProjectId, WebhookId, WebhookDeliveryId) {
        (self.project.id(), self.webhook, self.delivery)
    }
}

/// `FG-INT-0010 reason=owner_lost`, naming the owner and what
/// `authorize()` answered (never the URL).
fn owner_lost(material: &WebhookMaterial, decision: &ForgeError) -> ForgeError {
    ForgeError::new(ErrorCode::WebhookTargetForbidden)
        .with_context("reason", "owner_lost")
        .with_context("webhook_id", material.webhook.get())
        .with_context("project_id", material.project.get())
        .with_context("owner_id", material.owner.get())
        .with_context("decision", decision.code().id())
}

/// What webhook `material.webhook` may carry now (module docs). A gone
/// project is the owner having lost it too.
pub fn prove_webhook_delivery(
    material: &WebhookMaterial,
) -> Result<WebhookDeliverySet, ForgeError> {
    // The owner's standing, not a request's credential: a session's rights
    // (a person's administrator rights count, as when they created it).
    let principal = Principal::authenticated(
        material.owner,
        material.owner_admin,
        material.owner_active,
        Credential::Session,
        material.owner_kind,
    );
    let Some(access) = material.access else {
        let gone = ForgeError::new(ErrorCode::NotFound).with_context("reason", "project_gone");
        return Err(owner_lost(material, &gone));
    };
    let project = ProjectView {
        visibility: access.visibility,
        role: access.role,
        pending_deletion: access.pending_deletion,
    };
    authorize(
        &principal,
        Action::WebhookManage,
        &Resource::Project(project),
    )
    .map_err(|e| owner_lost(material, &e))?;
    let allowed: Vec<VisibilityClass> = VisibilityClass::ALL
        .into_iter()
        .filter(|class| {
            authorize(
                &principal,
                Action::WebhookDeliver,
                &Resource::ProjectEvents {
                    project,
                    class: *class,
                },
            )
            .is_ok()
        })
        .collect();
    // The admin class is never a webhook's: an allow is a bug in
    // authorize(), refused rather than delivered (fail closed).
    if allowed.contains(&VisibilityClass::Admin) {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "webhook_admin_class_allowed")
            .with_context("webhook_id", material.webhook.get()));
    }
    Ok(WebhookDeliverySet::from_authorized_decisions(
        material.webhook,
        material.project,
        &allowed,
        &material.event_types,
    ))
}
