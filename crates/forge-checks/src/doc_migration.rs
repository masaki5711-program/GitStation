//! The document lint's migration setting (plan 13 Q13, ADR-0026): the
//! commands and the read of `GET|PUT|DELETE
//! /api/v1/projects/{id}/doc-lint/migration`, on the route's proof.
//!
//! **Every write is decided twice** (as [`crate::Checks`]): the route
//! proves `DocLintMigrationManage` on the project its loader read for the
//! caller; forge-db's `set_doc_lint_migration_decided` /
//! `clear_doc_lint_migration_decided` lock the project, the caller's roles
//! and account and call `decide` — here
//! `Authorized::reconfirm` on those facts (a Maintainer removed meanwhile
//! is refused). Only this crate calls them (`tests/decided_callers.rs`).
//!
//! The date's shape (`YYYY-MM-DD`, a calendar date) is checked here
//! (`FG-WEB-0006 param=warn_until`); its bounds (after today, at most
//! `MAX_DAYS` ahead) by forge-db against the database's date
//! (`FG-DOC-0005`).

use forge_auth::{Action, Authorized, ProjectCtx};
use forge_core::doc_lint_migration::is_calendar_date;
use forge_db::checks::doc_lint_migration::{self as db, MigrationRead, SetOutcome};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use crate::{account, caller, change_context, loaded, require, ChecksRequest};

/// The setting's commands and read (`forge serve` builds one).
#[derive(Clone)]
pub struct DocLintMigrations {
    db: Db,
}

impl std::fmt::Debug for DocLintMigrations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocLintMigrations").finish_non_exhaustive()
    }
}

/// A date that is not `YYYY-MM-DD` or no calendar date.
fn date_invalid() -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "param_invalid")
        .with_context("param", "warn_until")
        .with_context("rule", "not_a_date")
}

impl DocLintMigrations {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// `GET …/doc-lint/migration` (`ProjectRead`): the setting, if any,
    /// with the database's date and the latest date a new setting may name.
    pub async fn get(&self, proof: &Authorized<ProjectCtx>) -> Result<MigrationRead, ForgeError> {
        require(proof.action(), Action::ProjectRead)?;
        db::read(&self.db, *proof.id()).await
    }

    /// `PUT …/doc-lint/migration` (`DocLintMigrationManage`): set the date.
    /// Errors (nothing stored): `FG-WEB-0006 param=warn_until` (the shape),
    /// `FG-DOC-0005` (the bounds), the decision's.
    pub async fn set(
        &self,
        request: &ChecksRequest,
        proof: &Authorized<ProjectCtx>,
        warn_until: &str,
    ) -> Result<(SetOutcome, MigrationRead), ForgeError> {
        require(proof.action(), Action::DocLintMigrationManage)?;
        if !is_calendar_date(warn_until) {
            return Err(date_invalid());
        }
        let user = caller(proof.principal(), Action::DocLintMigrationManage)?;
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        let (outcome, ()) = db::set_doc_lint_migration_decided(
            &self.db,
            &ctx,
            project,
            user,
            warn_until,
            |facts| {
                let fresh = ProjectCtx::new(loaded(&facts.project));
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        tracing::info!(
            target: "forge_checks",
            event = "forge.doc_lint.migration_set",
            request_id = %request.request_id,
            project_id = project.get(),
            warn_until,
            outcome = ?outcome,
            "broken document references are warnings until the date"
        );
        Ok((outcome, db::read(&self.db, project).await?))
    }

    /// `DELETE …/doc-lint/migration` (`DocLintMigrationManage`): broken
    /// references are errors again now. `FG-REPO-0005
    /// resource=doc_lint_migration` when there is no setting.
    pub async fn clear(
        &self,
        request: &ChecksRequest,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::DocLintMigrationManage)?;
        let user = caller(proof.principal(), Action::DocLintMigrationManage)?;
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        db::clear_doc_lint_migration_decided(&self.db, &ctx, project, user, |facts| {
            let fresh = ProjectCtx::new(loaded(&facts.project));
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_date_that_is_not_one_is_request_invalid_on_warn_until() {
        let e = date_invalid();
        assert_eq!(e.code(), ErrorCode::RequestInvalid);
        assert!(e
            .context()
            .iter()
            .any(|(k, v)| *k == "param" && v == "warn_until"));
    }
}
