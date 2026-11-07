//! The push mirror's administration (M3 O2; plan 11 §5 O2; ADR-0024
//! Amendment O2; runbook `gitlab-mirror`): the commands and the read of
//! `GET|PUT|DELETE /api/v1/projects/{id}/push-mirror` and `POST
//! …/push-mirror/sync`, on the route's proof.
//!
//! **Every write is decided twice** (as [`crate::webhooks::Webhooks`]): the
//! route proves `PushMirrorManage` on the project the loader read for the
//! caller; forge-db's `set_decided` / `remove_decided` lock the project,
//! the caller's roles and account and call `decide` — here
//! `Authorized::reconfirm` on those facts (an administrator whose rights
//! were taken away meanwhile is refused). Only this crate calls them
//! (`tests/decided_callers.rs`).
//!
//! A destination passes the operator's mirror policy after the decision
//! (`FG-INT-0021`: the shape and `mirror.allowed_hosts`), its credential
//! name the name rules, and the credential file must be usable now (a
//! regular file of `mirror.credentials_dir`, mode 0600, one
//! `<username>:<token>` line) — so a setting that could never push is
//! refused at once instead of failing in the worker. No error, log or
//! audit row carries the URL's path, the token or the file's content: the
//! origin (`scheme://host:port`) and the credential name only.

use std::sync::Arc;

use forge_auth::{AccountFacts, Action, Authorized, LoadedProject, Principal, ProjectCtx};
use forge_core::config::{CredentialProblem, MirrorConfig};
use forge_core::mirror::CredentialName;
use forge_core::{RequestId, UserId};
use forge_db::access::Account;
use forge_db::change::{Actor, ChangeContext};
use forge_db::jobs::Enqueued;
// Imported under another name: forge-hosting's guard (its
// tests/decided_callers.rs) would read the call as its `move` command's.
use forge_db::mirror::{self as db, remove_decided as remove_mirror_decided, MirrorSetting};
use forge_db::projects::ProjectAccessFacts;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

pub use forge_db::mirror::{MirrorStatus, PushMirror, SetOutcome};

/// The request a write belongs to (the audit row's request id and token).
#[derive(Debug, Clone)]
pub struct MirrorRequest {
    pub request_id: RequestId,
    /// The personal access token used (`None` for a session).
    pub credential_id: Option<i64>,
}

/// `PUT …/push-mirror`: the destination as sent.
#[derive(Debug, Clone)]
pub struct MirrorInput {
    pub url: String,
    pub credential_name: String,
    pub enabled: bool,
}

/// The push mirror commands and read (`forge serve` builds one).
#[derive(Clone)]
pub struct PushMirrors {
    db: Db,
    config: Arc<MirrorConfig>,
}

impl std::fmt::Debug for PushMirrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushMirrors").finish_non_exhaustive()
    }
}

/// `FG-SYS-0001 reason=proof_action_mismatch` unless the proof is for
/// `expected`.
fn require(action: Action, expected: Action) -> Result<(), ForgeError> {
    if action == expected {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "proof_action_mismatch")
        .with_context("command_action", expected.name())
        .with_context("proof_action", action.name()))
}

fn caller(principal: &Principal) -> Result<UserId, ForgeError> {
    principal.user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_without_user")
            .with_context("action", Action::PushMirrorManage.name())
    })
}

fn change_context(request: &MirrorRequest, principal: &Principal, user: UserId) -> ChangeContext {
    // Managing a mirror is a person's (`people_only`): always a user.
    let mut ctx = ChangeContext::new(Actor::User(user), request.request_id.clone()); // clone: the context owns it
    ctx.credential_id = request.credential_id;
    // Never an act-as principal (a mirror is not acted for, ADR-0025 §4).
    ctx.effective_scope = principal.scope_names();
    ctx
}

fn account(a: Account) -> AccountFacts {
    AccountFacts {
        active: a.active,
        is_admin: a.is_admin,
    }
}

fn loaded(facts: &ProjectAccessFacts) -> LoadedProject {
    LoadedProject {
        id: facts.project,
        visibility: facts.access.visibility,
        role: facts.access.role,
        pending_deletion: facts.access.pending_deletion,
    }
}

fn absent() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "push_mirror")
        .with_context("reason", "absent")
}

/// What went wrong underneath: `e`'s `detail` (the credential line's
/// shape), else its `reason` (the file's check).
fn reason_of(e: &ForgeError) -> Option<String> {
    let ctx = e.context();
    let get = |key: &str| {
        ctx.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone()) // clone: the new error owns it
    };
    get("detail").or_else(|| get("reason"))
}

/// A credential that cannot be used, as the route answers it
/// (`FG-INT-0021`; the file's content and path never in it — only the
/// name, the problem and the underlying reason).
fn credential_refused(
    name: &CredentialName,
    problem: CredentialProblem,
    cause: &ForgeError,
) -> ForgeError {
    let reason = match problem {
        CredentialProblem::NotConfigured | CredentialProblem::Missing => "credential_missing",
        CredentialProblem::Insecure => "credential_insecure",
        CredentialProblem::Invalid => "credential_invalid",
    };
    let mut e = ForgeError::new(ErrorCode::PushMirrorTargetInvalid)
        .with_context("reason", reason)
        .with_context("credential_name", name.as_str());
    let detail = match problem {
        CredentialProblem::NotConfigured => Some("not_configured".to_owned()),
        CredentialProblem::Missing | CredentialProblem::Insecure | CredentialProblem::Invalid => {
            reason_of(cause)
        }
    };
    if let Some(detail) = detail {
        e = e.with_context("detail", detail);
    }
    e
}

impl PushMirrors {
    pub fn new(db: Db, config: MirrorConfig) -> Self {
        Self {
            db,
            config: Arc::new(config),
        }
    }

    /// The mirror of the proof's project, or `FG-REPO-0005
    /// resource=push_mirror`.
    async fn stored(&self, proof: &Authorized<ProjectCtx>) -> Result<PushMirror, ForgeError> {
        db::get(&self.db, *proof.id()).await?.ok_or_else(absent)
    }

    /// `GET …/push-mirror` (`PushMirrorRead`): the mirror and its status
    /// (`FG-REPO-0005 resource=push_mirror` when there is none).
    pub async fn get(&self, proof: &Authorized<ProjectCtx>) -> Result<PushMirror, ForgeError> {
        require(proof.action(), Action::PushMirrorRead)?;
        self.stored(proof).await
    }

    /// The setting as checked now: the URL by the policy, the credential
    /// name by its rules, the credential file read once (module docs).
    fn checked(&self, input: &MirrorInput) -> Result<MirrorSetting, ForgeError> {
        let url = self.config.policy.check(&input.url)?;
        let name = CredentialName::parse(&input.credential_name)?;
        if let Err((problem, cause)) = self.config.credential(&name) {
            return Err(credential_refused(&name, problem, &cause));
        }
        Ok(MirrorSetting {
            url: url.as_str().to_owned(),
            origin: url.origin(),
            credential_name: name.as_str().to_owned(),
            enabled: input.enabled,
        })
    }

    /// `PUT …/push-mirror` (`PushMirrorManage`): create or change the
    /// mirror (a new or re-enabled one is pushed at once by the worker).
    /// Errors (nothing stored): `FG-INT-0021` (the destination, the
    /// credential, `no_repository`), the decision's.
    pub async fn set(
        &self,
        request: &MirrorRequest,
        proof: &Authorized<ProjectCtx>,
        input: &MirrorInput,
    ) -> Result<(SetOutcome, PushMirror), ForgeError> {
        require(proof.action(), Action::PushMirrorManage)?;
        let user = caller(proof.principal())?;
        let project = *proof.id();
        let setting = self.checked(input)?;
        let ctx = change_context(request, proof.principal(), user);
        let (outcome, ()) = db::set_decided(&self.db, &ctx, project, user, &setting, |facts| {
            let fresh = ProjectCtx::new(loaded(&facts.project));
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await?;
        let stored = db::get(&self.db, project).await?.ok_or_else(|| {
            // Written by this request and gone before the answer: only a
            // concurrent removal can do it.
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "push_mirror_vanished")
                .with_context("project_id", project.get())
        })?;
        Ok((outcome, stored))
    }

    /// `DELETE …/push-mirror` (`PushMirrorManage`): `FG-REPO-0005
    /// resource=push_mirror` when there is none.
    pub async fn remove(
        &self,
        request: &MirrorRequest,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::PushMirrorManage)?;
        let user = caller(proof.principal())?;
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        remove_mirror_decided(&self.db, &ctx, project, user, |facts| {
            let fresh = ProjectCtx::new(loaded(&facts.project));
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await
    }

    /// `POST …/push-mirror/sync` (`PushMirrorManage`): one push now.
    /// `true` when a job was queued, `false` when the same request's job
    /// was already there. No enabled mirror: `FG-REPO-0005
    /// resource=push_mirror`.
    pub async fn sync(
        &self,
        request: &MirrorRequest,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<bool, ForgeError> {
        require(proof.action(), Action::PushMirrorManage)?;
        let mirror = self.stored(proof).await?;
        let user = caller(proof.principal())?;
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        // Decided again in the transaction (authz review: an administrator
        // who lost the rights after the route's decision queues nothing).
        let (enqueued, ()) = db::sync_decided(&self.db, &ctx, project, user, |facts| {
            let fresh = ProjectCtx::new(loaded(&facts.project));
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await?;
        let queued = match enqueued {
            Enqueued::Queued(_) => true,
            Enqueued::Deduped => false,
        };
        tracing::info!(
            target: "forge_integ",
            event = "forge.mirror.sync_requested",
            request_id = %request.request_id,
            project_id = mirror.project.get(),
            repo_id = mirror.repo.get(),
            origin = %mirror.origin,
            queued,
            "an administrator asked for a mirror push now"
        );
        Ok(queued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_problem_is_int_0021_with_its_reason_and_never_the_path() {
        let name = CredentialName::parse("gitlab").expect("name");
        let cause = ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("reason", "insecure_permissions")
            .with_context("path", "/etc/forge/mirror/gitlab");
        for (problem, reason, detail) in [
            (
                CredentialProblem::NotConfigured,
                "credential_missing",
                "not_configured",
            ),
            (
                CredentialProblem::Missing,
                "credential_missing",
                "insecure_permissions",
            ),
            (
                CredentialProblem::Insecure,
                "credential_insecure",
                "insecure_permissions",
            ),
            (
                CredentialProblem::Invalid,
                "credential_invalid",
                "insecure_permissions",
            ),
        ] {
            let e = credential_refused(&name, problem, &cause);
            assert_eq!(e.code(), ErrorCode::PushMirrorTargetInvalid);
            let ctx = e.context();
            let get = |k: &str| {
                ctx.iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| v.as_str())
            };
            assert_eq!(get("reason"), Some(reason), "{problem:?}");
            assert_eq!(get("detail"), Some(detail), "{problem:?}");
            assert_eq!(get("credential_name"), Some("gitlab"));
            assert_eq!(get("path"), None);
        }
    }
}
