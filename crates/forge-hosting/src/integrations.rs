//! The integration routes' commands and reads (M3 I1a, plan 11 §5 I1,
//! ADR-0016 Amendment I1a), on the proofs the route registry made.
//!
//! Each command takes the proof of its own action (another action is
//! `FG-SYS-0001 reason=proof_action_mismatch`) and acts on the ids the
//! proof carries. The administration's commands (`IntegrationManage`) are
//! decided once, on the instance: what they touch is the integration row
//! alone, which the forge-db command locks. A binding is a membership
//! change of the project, so it is decided twice like every project write
//! (crate docs): forge-db's `bind_decided` / `unbind_decided` lock the
//! project, re-read the caller's roles and account, and call
//! [`forge_auth::Authorized::reconfirm`] before anything is written.

use forge_auth::{
    Action, Authorized, InstanceCtx, IntegrationBindingCtx, IntegrationCtx, LoadedProject,
};
use forge_core::{IntegrationId, ProjectId};
use forge_db::integrations::{self, BindingFacts, Changes, NewIntegration};
use forge_errors::ForgeError;

use crate::{account, caller, change_context, require, Hosting, WriteRequest};

pub use forge_db::integrations::{
    BindOutcome, Binding as IntegrationBinding, Changes as IntegrationChanges, Integration,
    Listed as Integrations, NewIntegration as IntegrationFields, UnbindOutcome,
    UpdateOutcome as IntegrationUpdate, MAX_BINDINGS, MAX_LISTED as MAX_LISTED_INTEGRATIONS,
};

fn fresh(facts: &BindingFacts, integration: IntegrationId) -> IntegrationBindingCtx {
    IntegrationBindingCtx::new(
        LoadedProject {
            id: facts.project.project,
            visibility: facts.project.access.visibility,
            role: facts.project.access.role,
            pending_deletion: facts.project.access.pending_deletion,
        },
        integration,
    )
}

impl Hosting {
    /// Whether integration `id` exists, for the routes' loaders.
    pub async fn integration_exists(&self, id: IntegrationId) -> Result<bool, ForgeError> {
        Ok(integrations::get(&self.db, id).await?.is_some())
    }

    /// Create an integration (`IntegrationManage`, an administrator's
    /// session): the proof's caller is its creator.
    pub async fn create_integration(
        &self,
        proof: &Authorized<InstanceCtx>,
        request: &WriteRequest,
        new: &NewIntegration,
    ) -> Result<IntegrationId, ForgeError> {
        require(proof.action(), Action::IntegrationManage)?;
        let user = caller(proof)?;
        integrations::create(
            &self.db,
            &change_context(request, proof.principal(), user),
            new,
            user,
        )
        .await
    }

    /// The integration [`Hosting::create_integration`] just created, for its
    /// `201` answer (the proof is the create's).
    pub async fn created_integration(
        &self,
        proof: &Authorized<InstanceCtx>,
        id: IntegrationId,
    ) -> Result<Option<Integration>, ForgeError> {
        require(proof.action(), Action::IntegrationManage)?;
        integrations::get(&self.db, id).await
    }

    /// Every integration (`IntegrationRead`).
    pub async fn integrations(
        &self,
        proof: &Authorized<InstanceCtx>,
    ) -> Result<Integrations, ForgeError> {
        require(proof.action(), Action::IntegrationRead)?;
        integrations::list(&self.db).await
    }

    /// The proof's integration (`IntegrationRead`, or after
    /// `IntegrationManage` to answer with the stored state); `None` when it
    /// vanished after the loader read it.
    pub async fn integration(
        &self,
        proof: &Authorized<IntegrationCtx>,
    ) -> Result<Option<Integration>, ForgeError> {
        if proof.action() != Action::IntegrationManage {
            require(proof.action(), Action::IntegrationRead)?;
        }
        integrations::get(&self.db, *proof.id()).await
    }

    /// The projects bound to the proof's integration (`IntegrationRead`).
    pub async fn integration_bindings(
        &self,
        proof: &Authorized<IntegrationCtx>,
    ) -> Result<Vec<IntegrationBinding>, ForgeError> {
        require(proof.action(), Action::IntegrationRead)?;
        integrations::bindings(&self.db, *proof.id()).await
    }

    /// Change the proof's integration (`IntegrationManage`).
    pub async fn update_integration(
        &self,
        proof: &Authorized<IntegrationCtx>,
        request: &WriteRequest,
        changes: &Changes,
    ) -> Result<IntegrationUpdate, ForgeError> {
        require(proof.action(), Action::IntegrationManage)?;
        let user = caller(proof)?;
        integrations::update(
            &self.db,
            &change_context(request, proof.principal(), user),
            *proof.id(),
            changes,
        )
        .await
    }

    /// The binding of a project the caller may read (`ProjectRead` on the
    /// project route, decided by the registry); none or one.
    pub async fn project_integration_bindings(
        &self,
        proof: &Authorized<forge_auth::ProjectCtx>,
    ) -> Result<Vec<IntegrationBinding>, ForgeError> {
        require(proof.action(), Action::ProjectRead)?;
        integrations::project_bindings(&self.db, *proof.id()).await
    }

    /// Bind the proof's project to `external_project_id` of the proof's
    /// integration (`IntegrationBindingManage`), decided again inside the
    /// transaction.
    pub async fn bind_integration(
        &self,
        proof: &Authorized<IntegrationBindingCtx>,
        request: &WriteRequest,
        external_project_id: &str,
    ) -> Result<BindOutcome, ForgeError> {
        require(proof.action(), Action::IntegrationBindingManage)?;
        let user = caller(proof)?;
        let (project, integration): (ProjectId, IntegrationId) = *proof.id();
        let (outcome, ()) = integrations::bind_decided(
            &self.db,
            &change_context(request, proof.principal(), user),
            project,
            user,
            integration,
            external_project_id,
            |facts| {
                proof
                    .reconfirm(account(facts.account), fresh(facts, integration))
                    .map(drop)
            },
        )
        .await?;
        Ok(outcome)
    }

    /// The proof's project's binding as stored, for the answer of
    /// [`Hosting::bind_integration`] (the proof is the bind's).
    pub async fn bound_integration(
        &self,
        proof: &Authorized<IntegrationBindingCtx>,
    ) -> Result<Vec<IntegrationBinding>, ForgeError> {
        require(proof.action(), Action::IntegrationBindingManage)?;
        let (project, _) = *proof.id();
        integrations::project_bindings(&self.db, project).await
    }

    /// Unbind the proof's project from the proof's integration
    /// (`IntegrationBindingManage`), decided again inside the transaction.
    pub async fn unbind_integration(
        &self,
        proof: &Authorized<IntegrationBindingCtx>,
        request: &WriteRequest,
    ) -> Result<UnbindOutcome, ForgeError> {
        require(proof.action(), Action::IntegrationBindingManage)?;
        let user = caller(proof)?;
        let (project, integration): (ProjectId, IntegrationId) = *proof.id();
        let (outcome, ()) = integrations::unbind_decided(
            &self.db,
            &change_context(request, proof.principal(), user),
            project,
            user,
            integration,
            |facts| {
                proof
                    .reconfirm(account(facts.account), fresh(facts, integration))
                    .map(drop)
            },
        )
        .await?;
        Ok(outcome)
    }
}
