//! Deciding a whole copy of a repository (plan 06 D-5, ADR-0026 decision
//! of 2026-11-10): a clone or fetch over Smart HTTP (the upload-pack
//! advertisement and RPC) and an archive. [`authorize_repo_copy`] calls
//! [`authorize`] once with [`Resource::RepoCopy`], so the rule — an AI
//! agent audience copies a repository only when its default branch's tip
//! has no confidential file — lives in the single decision point; this
//! holds no rule of its own.
//!
//! It takes the proof of the project-level `RepoRead` decision (made by
//! the dispatcher) and the route's measurement, and nothing else about the
//! caller: the principal and the project view are the proof's, so a
//! handler cannot decide the copy for another caller or another project.

use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::authorize::authorize;
use crate::model::{CopyMark, Resource};
use crate::prove::{Authorized, ProjectCtx};

/// `authorize(<the proof's principal>, RepoRead, RepoCopy { <the proof's
/// project>, mark })`. `FG-SYS-0001` (a programming error, never a pass)
/// when `project` is not a `RepoRead` proof (`reason=proof_action_mismatch`).
/// The refusal of an agent audience is `FG-AUTH-0022` with `reason` =
/// [`CopyMark::as_str`].
pub fn authorize_repo_copy(
    project: &Authorized<ProjectCtx>,
    mark: CopyMark,
) -> Result<(), ForgeError> {
    if project.action() != Action::RepoRead {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_action_mismatch")
            .with_context("route_action", Action::RepoRead.name())
            .with_context("proof_action", project.action().name()));
    }
    authorize(
        project.principal(),
        Action::RepoRead,
        &Resource::RepoCopy {
            project: project.ctx().view(),
            mark,
        },
    )
    .map_err(|e| e.with_context("project_id", project.id().get()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

    use super::*;
    use crate::model::{AccountKind, Credential, Principal, Role, Scope, Visibility};
    use crate::prove::{prove, LoadedProject};
    use forge_core::{ProjectId, UserId};

    fn proof(principal: Principal, action: Action) -> Authorized<ProjectCtx> {
        let ctx = ProjectCtx::new(LoadedProject {
            id: ProjectId::new(7),
            visibility: Visibility::Private,
            role: Some(Role::Developer),
            pending_deletion: false,
        });
        prove(&principal, action, ctx).expect("allowed")
    }

    fn token(kind: AccountKind) -> Principal {
        Principal::authenticated(
            UserId::new(1),
            false,
            true,
            Credential::Token(vec![Scope::ReadRepository, Scope::WriteRepository]),
            kind,
        )
    }

    #[test]
    fn the_proof_decides_for_its_own_principal_and_project_only() {
        let agent = token(AccountKind::Human).as_agent(true);
        let read = proof(agent.clone(), Action::RepoRead);
        assert!(authorize_repo_copy(&read, CopyMark::Open).is_ok());
        let err = authorize_repo_copy(&read, CopyMark::Confidential).expect_err("agent");
        assert_eq!(err.code(), ErrorCode::RepositoryCopyWithheld);
        assert!(err
            .context()
            .iter()
            .any(|(k, v)| *k == "project_id" && v == "7"));
        let push = proof(agent, Action::RepoPush);
        let err = authorize_repo_copy(&push, CopyMark::Open).expect_err("wrong proof");
        assert_eq!(err.code(), ErrorCode::Internal);
    }

    #[test]
    fn a_person_is_never_measured_and_never_refused() {
        let person = proof(token(AccountKind::Human), Action::RepoRead);
        for mark in [
            CopyMark::NotMeasured,
            CopyMark::Open,
            CopyMark::Confidential,
            CopyMark::Unborn,
            CopyMark::Undecided,
        ] {
            assert!(authorize_repo_copy(&person, mark).is_ok(), "{mark:?}");
        }
    }
}
