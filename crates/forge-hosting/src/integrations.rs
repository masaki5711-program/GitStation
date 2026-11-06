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
    Action, Authorized, BindingSubject, InstanceCtx, IntegrationBindingCtx, IntegrationCtx,
    IntegrationFeedCtx, LoadedProject,
};
use forge_core::{IntegrationId, ProjectId, ProjectRecord};
use forge_db::integrations::{self, BindingFacts, Changes, NewBinding, NewIntegration};
use forge_errors::{ErrorCode, ForgeError};

use crate::{account, act_as_use, caller, change_context, require, Hosting, WriteRequest};

pub use forge_db::integrations::{
    BindOutcome, Binding as IntegrationBinding, BindingUpdate, BoundRepository,
    Changes as IntegrationChanges, Integration, Listed as Integrations,
    NewIntegration as IntegrationFields, UnbindOutcome, UpdateOutcome as IntegrationUpdate,
    MAX_BINDINGS, MAX_LISTED as MAX_LISTED_INTEGRATIONS,
};

/// The most candidates a repository search reads and decides (M3 X3).
pub const SEARCH_CANDIDATES: u32 = 200;

/// The reconfirmation's context under the lock: the project as locked, the
/// integration, and — for an act-as proof — the binding the request writes
/// as the proof's own context named it (its assertion is compared with it
/// again).
fn fresh(
    facts: &BindingFacts,
    proof: &Authorized<IntegrationBindingCtx>,
    integration: IntegrationId,
) -> IntegrationBindingCtx {
    let ctx = IntegrationBindingCtx::new(
        LoadedProject {
            id: facts.project.project,
            visibility: facts.project.access.visibility,
            role: facts.project.access.role,
            pending_deletion: facts.project.access.pending_deletion,
        },
        integration,
    );
    match acting_subject(proof, facts.project.project) {
        Some(subject) => ctx.acting(subject),
        None => ctx,
    }
}

/// The binding an act-as proof's context was decided with, for the
/// locked project (`None` for any other principal).
fn acting_subject(
    proof: &Authorized<IntegrationBindingCtx>,
    project: ProjectId,
) -> Option<BindingSubject> {
    proof.principal().act_as_credential()?;
    proof.ctx().acting_subject().map(|b| BindingSubject {
        project,
        ..b.clone() // clone: the fresh context owns its facts
    })
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

    /// Whether `project` has an integration binding that resolves work
    /// items (M3 I1d: the merge-rule write's `work_item_link_unbound`
    /// warning — M3 X3: a shared spec project links no work items; the
    /// caller already holds the merge-rule proof on the project).
    pub async fn project_is_bound(&self, project: ProjectId) -> Result<bool, ForgeError> {
        Ok(integrations::project_bindings(&self.db, project)
            .await?
            .iter()
            .any(|b| b.resolves_work_items))
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
    /// transaction. The binding as the transaction wrote (or found) it is
    /// the answer: no read after the commit, which a concurrent unbind
    /// could empty.
    pub async fn bind_integration(
        &self,
        proof: &Authorized<IntegrationBindingCtx>,
        request: &WriteRequest,
        external_project_id: &str,
    ) -> Result<(BindOutcome, IntegrationBinding), ForgeError> {
        require(proof.action(), Action::IntegrationBindingManage)?;
        let user = caller(proof)?;
        let (project, integration): (ProjectId, IntegrationId) = *proof.id();
        let (outcome, binding, ()) = integrations::bind_decided(
            &self.db,
            &change_context(request, proof.principal(), user),
            project,
            user,
            integration,
            external_project_id,
            |facts| {
                proof
                    .reconfirm(account(facts.account), fresh(facts, proof, integration))
                    .map(drop)
            },
        )
        .await?;
        Ok((outcome, binding))
    }

    /// Bind the proof's project to `external_project_id` of the proof's
    /// integration with the setting `resolves_work_items` (M3 X3, `POST
    /// /api/v1/integrations/{id}/bindings`; `IntegrationBindingManage`),
    /// decided again inside the transaction; an act-as request's assertion
    /// is consumed there.
    pub async fn bind_repository(
        &self,
        proof: &Authorized<IntegrationBindingCtx>,
        request: &WriteRequest,
        external_project_id: &str,
        resolves_work_items: bool,
    ) -> Result<(BindOutcome, IntegrationBinding), ForgeError> {
        require(proof.action(), Action::IntegrationBindingManage)?;
        let user = caller(proof)?;
        let (project, integration): (ProjectId, IntegrationId) = *proof.id();
        let using = act_as_use(proof.principal());
        let (outcome, binding, ()) = integrations::bind_as_decided(
            &self.db,
            &change_context(request, proof.principal(), user),
            project,
            user,
            &NewBinding {
                integration,
                external_project_id,
                resolves_work_items,
            },
            using.as_ref(),
            |facts| {
                proof
                    .reconfirm(account(facts.account), fresh(facts, proof, integration))
                    .map(drop)
            },
        )
        .await?;
        Ok((outcome, binding))
    }

    /// Change the proof's binding's setting (M3 X3, `PATCH
    /// …/bindings/{binding_id}`; `IntegrationBindingManage`).
    pub async fn change_binding(
        &self,
        proof: &Authorized<IntegrationBindingCtx>,
        request: &WriteRequest,
        resolves_work_items: bool,
    ) -> Result<(BindingUpdate, IntegrationBinding), ForgeError> {
        require(proof.action(), Action::IntegrationBindingManage)?;
        let user = caller(proof)?;
        let (project, integration): (ProjectId, IntegrationId) = *proof.id();
        let binding = proof.ctx().binding().ok_or_else(binding_missing)?;
        let using = act_as_use(proof.principal());
        let (outcome, stored, ()) = integrations::update_binding_decided(
            &self.db,
            &change_context(request, proof.principal(), user),
            project,
            user,
            integration,
            binding,
            resolves_work_items,
            using.as_ref(),
            |facts| {
                proof
                    .reconfirm(account(facts.account), fresh(facts, proof, integration))
                    .map(drop)
            },
        )
        .await?;
        Ok((outcome, stored))
    }

    /// Remove the proof's binding (M3 X3, `DELETE …/bindings/{binding_id}`;
    /// `IntegrationBindingRemove`).
    pub async fn remove_binding(
        &self,
        proof: &Authorized<IntegrationBindingCtx>,
        request: &WriteRequest,
    ) -> Result<UnbindOutcome, ForgeError> {
        require(proof.action(), Action::IntegrationBindingRemove)?;
        let user = caller(proof)?;
        let (project, integration): (ProjectId, IntegrationId) = *proof.id();
        let binding = proof.ctx().binding().ok_or_else(binding_missing)?;
        let using = act_as_use(proof.principal());
        let (outcome, ()) = integrations::unbind_binding_decided(
            &self.db,
            &change_context(request, proof.principal(), user),
            project,
            user,
            integration,
            binding,
            using.as_ref(),
            |facts| {
                proof
                    .reconfirm(account(facts.account), fresh(facts, proof, integration))
                    .map(drop)
            },
        )
        .await?;
        Ok(outcome)
    }

    /// Integration `id` as its own service account's routes load it (M3
    /// X3, `IntegrationBindingList`; `None`: absent).
    pub async fn integration_feed_target(
        &self,
        id: IntegrationId,
    ) -> Result<Option<forge_auth::LoadedIntegrationFeed>, ForgeError> {
        Ok(integrations::get(&self.db, id)
            .await?
            .map(|i| forge_auth::LoadedIntegrationFeed {
                integration: i.id,
                service_account: i.service_account,
                enabled: i.enabled,
            }))
    }

    /// Binding `id` of `integration`, for the binding routes' loaders
    /// (`None`: absent or another integration's).
    pub async fn integration_binding(
        &self,
        integration: IntegrationId,
        id: i64,
    ) -> Result<Option<IntegrationBinding>, ForgeError> {
        integrations::binding(&self.db, integration, id).await
    }

    /// The repositories bound to the proof's integration — to kanflow
    /// project `external` when given (M3 X3, `IntegrationBindingList`: the
    /// integration's own service account).
    pub async fn integration_repositories(
        &self,
        proof: &Authorized<IntegrationFeedCtx>,
        external: Option<&str>,
    ) -> Result<Vec<BoundRepository>, ForgeError> {
        require(proof.action(), Action::IntegrationBindingList)?;
        integrations::external_bindings(&self.db, *proof.id(), external).await
    }

    /// The candidates of a repository search for the proof's caller (the
    /// person, acting for one): live projects whose path or name contains
    /// `q` (ASCII case folded), at most [`SEARCH_CANDIDATES`], with the
    /// caller's access facts — every one still to be decided by the route
    /// (`RepositorySearch` on each project; M3 X3).
    pub async fn repository_candidates(
        &self,
        proof: &Authorized<InstanceCtx>,
        q: &str,
    ) -> Result<Vec<ProjectRecord>, ForgeError> {
        require(proof.action(), Action::RepositorySearch)?;
        let viewer = caller(proof)?;
        forge_db::lists::project_search(&self.db, viewer, q, SEARCH_CANDIDATES).await
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
                    .reconfirm(account(facts.account), fresh(facts, proof, integration))
                    .map(drop)
            },
        )
        .await?;
        Ok(outcome)
    }
}

/// A binding route's proof without its binding: a loader bug.
fn binding_missing() -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_binding")
}
