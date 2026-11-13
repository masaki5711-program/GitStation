//! CI variables — a project's secrets for its jobs (M4 C6, plan 14 §4.1,
//! §4.2; ADR-0029 decision 8 and obligation 5; ADR-0013).
//!
//! - **Manage** ([`Pipelines::variables`], [`Pipelines::set_variable`],
//!   [`Pipelines::delete_variable`], `CiVariableManage`: a Maintainer,
//!   people only): the value is checked
//!   (`forge_core::ci_secret::check_value`), sealed with the newest key of
//!   `ci.secret_keys_file` and written with its audit row, decided again
//!   on the locked project. No read ever answers a value.
//! - **Release** ([`Pipelines::release_secrets`], with a runner's claim):
//!   only when the stored pipeline says so (`StoredPipeline::
//!   secrets_allowed`: a push to a branch protected at the pipeline's
//!   creation — never a merge request's, a manual or a scheduled one). A
//!   job that asks while not allowed is `FG-CI-0017`; a name the project
//!   does not define `FG-CI-0018`; a key that is not configured, missing or
//!   does not open `FG-CI-0016`. Each fails the job in the claim's place
//!   (`lease::fail_claimed`): a job never runs without what it asked for.
//! - **Snapshot** (M4 C6 review): what a claim released is recorded with
//!   the job (`forge.ci_job_secret_snapshots`, `ci_jobs.secrets_released`)
//!   before the values leave forge; masking and the artifact scan use the
//!   snapshot, so a value changed or deleted mid-job stays masked.
//! - **Mask** ([`Pipelines::log_masks`]): before a log chunk is stored,
//!   every masked value the job may have been given (and its common
//!   encodings) is replaced by `[masked]`. A job with masked values whose
//!   keys are unavailable cannot store a chunk (`FG-CI-0016`): an unmasked
//!   chunk is never stored.
//!
//! Every failure here is logged once, by the caller that handles it
//! ([`crate::runners`]), with the code's event name; never a value.

use forge_auth::{Action, Authorized, ProjectCtx};
use forge_core::ci_secret::{self, key_unavailable, CiSecretKeys, VariableValue};
use forge_db::ci_runners::lease::Claimed;
use forge_db::ci_variables::{self as db, NewValue, StoredVariable};
use forge_errors::{ErrorCode, ForgeError};

use crate::{caller, change_context, reconfirm, require, ChangeRequest, Pipelines};

pub use forge_db::ci_variables::StoredVariable as Variable;

/// A value released to a claimed job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedSecret {
    pub name: String,
    pub value: VariableValue,
    pub masked: bool,
}

/// What a person asks to store.
#[derive(Debug)]
pub struct VariableInput {
    pub name: String,
    pub value: VariableValue,
    pub protected_only: bool,
    pub masked: bool,
}

fn param(param: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("param", param)
        .with_context("reason", reason)
}

/// Why `name` cannot be a variable's name (`FG-WEB-0006 param=name`).
pub fn check_name(name: &str) -> Result<(), ForgeError> {
    match ci_secret::check_name(name) {
        None => Ok(()),
        Some(reason) => Err(param("name", reason)),
    }
}

impl Pipelines {
    fn keys(&self) -> Result<&CiSecretKeys, ForgeError> {
        self.secret_keys
            .as_deref()
            .ok_or_else(|| key_unavailable("not_configured"))
    }

    /// `GET …/ci/variables` (`CiVariableManage`): names and flags.
    pub async fn variables(
        &self,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<Vec<StoredVariable>, ForgeError> {
        require(proof.action(), Action::CiVariableManage)?;
        db::list(&self.db, *proof.id()).await
    }

    /// `PUT …/ci/variables/{name}` (`CiVariableManage`): the variable after
    /// the change and whether it was created. Errors (nothing written):
    /// `FG-WEB-0006` (`param=name|value`, the reason; never the value),
    /// `FG-CI-0016` (no usable key), the reconfirmed decision's.
    pub async fn set_variable(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        input: &VariableInput,
    ) -> Result<(StoredVariable, bool), ForgeError> {
        require(proof.action(), Action::CiVariableManage)?;
        let user = caller(proof.principal(), Action::CiVariableManage)?;
        check_name(&input.name)?;
        if let Some(reason) = ci_secret::check_value(input.value.expose(), input.masked) {
            return Err(param("value", reason));
        }
        let project = *proof.id();
        let sealed = self.keys()?.seal(project, &input.name, &input.value)?;
        let new = NewValue {
            name: input.name.clone(), // clone: the row owns it
            sealed,
            value_bytes: i32::try_from(input.value.expose().len())
                .map_err(|_| param("value", "too_long"))?,
            protected_only: input.protected_only,
            masked: input.masked,
        };
        let ctx = change_context(request, proof.principal(), user);
        let (stored, created) = db::put_variable_decided(&self.db, &ctx, project, &new, |facts| {
            reconfirm(proof, facts)
        })
        .await?;
        tracing::info!(
            target: "forge_pipelines",
            event = "forge.ci.variable_set",
            request_id = %request.request_id,
            project_id = project.get(),
            name = %stored.name,
            created,
            protected_only = stored.protected_only,
            masked = stored.masked,
            key_id = stored.key_id,
            user_id = user.get(),
            "CI variable set"
        );
        Ok((stored, created))
    }

    /// `DELETE …/ci/variables/{name}` (`CiVariableManage`); `false`: no
    /// such variable.
    pub async fn delete_variable(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        name: &str,
    ) -> Result<bool, ForgeError> {
        require(proof.action(), Action::CiVariableManage)?;
        let user = caller(proof.principal(), Action::CiVariableManage)?;
        check_name(name)?;
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        let deleted = db::delete_variable_decided(&self.db, &ctx, project, name, |facts| {
            reconfirm(proof, facts)
        })
        .await?;
        if deleted {
            tracing::info!(
                target: "forge_pipelines",
                event = "forge.ci.variable_deleted",
                request_id = %request.request_id,
                project_id = project.get(),
                name = %name,
                user_id = user.get(),
                "CI variable deleted"
            );
        }
        Ok(deleted)
    }

    /// The values `claimed` asks for (module docs). Errors: `FG-CI-0017`
    /// (asked while not allowed; nothing read), `FG-CI-0018` (a name not
    /// defined, with `missing`), `FG-CI-0016` (keys). The caller fails the
    /// job with the code.
    pub async fn release_secrets(
        &self,
        claimed: &Claimed,
    ) -> Result<Vec<ReleasedSecret>, ForgeError> {
        if claimed.requested_secrets.is_empty() {
            return Ok(Vec::new());
        }
        // ADR-0029 obligation 5: the stored pipeline's answer alone.
        if !claimed.pipeline.secrets_allowed() {
            return Err(ForgeError::new(ErrorCode::SecretOnUnprotectedRef)
                .with_context("secrets", claimed.requested_secrets.join(",")));
        }
        let project = claimed.pipeline.project;
        let sealed = db::sealed_for(&self.db, project, &claimed.requested_secrets).await?;
        let missing: Vec<&str> = claimed
            .requested_secrets
            .iter()
            .filter(|n| !sealed.iter().any(|s| &s.name == *n))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(ForgeError::new(ErrorCode::SecretNotDefined)
                .with_context("missing", missing.join(",")));
        }
        let keys = self.keys()?;
        let released = sealed
            .iter()
            .map(|s| {
                let value = keys
                    .open(project, &s.name, &s.sealed)
                    .map_err(|e| e.with_context("name", s.name.clone()))?; // clone: the error names it
                Ok(ReleasedSecret {
                    name: s.name.clone(), // clone: the snapshot keeps its row
                    value,
                    masked: s.masked,
                })
            })
            .collect::<Result<Vec<_>, ForgeError>>()?;
        // M4 C6 review: the snapshot of what this attempt gets (masking
        // keeps it whatever happens to the variable) and the job's flag
        // (its artifacts are scanned and restricted). Recorded before any
        // value leaves forge.
        let recorded = db::record_release(
            &self.db,
            (project, claimed.job_id, claimed.attempt),
            &sealed,
        )
        .await?;
        if !recorded {
            return Err(ForgeError::new(ErrorCode::JobLeaseLost)
                .with_context("reason", "ended_before_release")
                .with_context("job_id", claimed.job_id));
        }
        Ok(released)
    }

    /// The forms `job`'s log is masked for before it is stored (module
    /// docs): empty when the job has no masked value. `FG-CI-0016` when it
    /// has one and the keys are not usable.
    pub async fn log_masks(&self, job: i64) -> Result<Vec<String>, ForgeError> {
        Ok(self.job_masks(job).await?.1)
    }

    /// Whether `job` was ever given secrets, and the forms of its masked
    /// values (the claim's snapshots, M4 C6 review): what its log is masked
    /// for and its artifacts are scanned for. `FG-CI-0016` when there are
    /// values and the keys are not usable.
    pub async fn job_masks(&self, job: i64) -> Result<(bool, Vec<String>), ForgeError> {
        let Some((project, released, sealed)) = db::masked_for_job(&self.db, job).await? else {
            return Ok((false, Vec::new()));
        };
        if sealed.is_empty() {
            return Ok((released, Vec::new()));
        }
        let keys = self.keys()?;
        let mut forms = Vec::new();
        for s in &sealed {
            let value = keys
                .open(project, &s.name, &s.sealed)
                .map_err(|e| e.with_context("name", s.name.clone()))?; // clone: the error names it
            forms.extend(ci_secret::mask_forms(value.expose()));
        }
        forms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        forms.dedup();
        Ok((released, forms))
    }
}
