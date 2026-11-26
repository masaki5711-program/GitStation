//! The improvement-draft token's arm of `authorize()` (plan 17 P17-5, §3,
//! ADR-0035 Amendment P17-5): `pfs-ai-bot`, PFS's integration, proposes
//! improvements as draft merge requests and does nothing else. Closed by
//! default, decided before the general rules:
//!
//! 1. no user or a disabled account: the general rules answer
//!    (`FG-AUTH-0001` / `FG-AUTH-0004`);
//! 2. a service account's token only (`reason=improve_draft_not_service_account`;
//!    forge-db issues it to no one else);
//! 3. a project it is no member of is the invisible project's 404 — an
//!    internal one included (`FG-REPO-0005 reason=improve_draft_not_member`):
//!    its membership is the `pfs-ai` group, so it sees nothing outside it;
//! 4. only [`allowed`] actions (`FG-AUTH-0002 reason=improve_draft_not_allowed`):
//!    reading the project, its repository (the browse, spec and context
//!    reads decided like it) and its merge requests, pushing,
//!    opening a merge request, commenting — never marking ready,
//!    approving, merging, closing, protected paths' settings, releases,
//!    deployments or anything administrative;
//! 5. a ref it pushes is a new `refs/heads/improve/*` branch
//!    (`reason=improve_branch_only`; `improve_draft_create_only`: no
//!    fast-forward, force or delete); a merge request it opens is a draft
//!    (`reason=draft_only`) from such a branch;
//! 6. then the general rules decide as for a Developer's `api` +
//!    `write_repository` token of the same account (role, protections,
//!    pending deletion, the agent audience's copy marks).
//!
//! What the push changes (no protected path, `forge_core::improve_draft`)
//! is the pre-receive hook's (`FG-GIT-0039`): the decision is made before
//! the objects arrive.

use forge_core::improve_draft::is_improve_branch;
use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::model::{AccountKind, Credential, Principal, RefUpdate, Resource, Scope};

/// What an improvement-draft token may ask for at all.
pub(crate) fn allowed(action: Action) -> bool {
    matches!(
        action,
        Action::PathResolve
            | Action::ProjectRead
            | Action::RepoRead
            | Action::RepoBrowse
            | Action::SpecRefRead
            | Action::SpecRead
            | Action::ContextRead
            | Action::RepoPush
            | Action::MergeRequestList
            | Action::MergeRequestRead
            | Action::MergeRequestCreate
            | Action::MergeRequestComment
    )
}

/// The scopes the general rules see for an allowed action.
const AS_SCOPES: [Scope; 4] = [
    Scope::ReadRepository,
    Scope::WriteRepository,
    Scope::ReadApi,
    Scope::Api,
];

fn refused(action: Action, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Forbidden)
        .with_context("action", action.name())
        .with_context("reason", reason)
}

pub(crate) fn decide(
    principal: &Principal,
    action: Action,
    resource: &Resource,
) -> Result<(), ForgeError> {
    // A public action is everyone's (a token's included).
    if action.is_public() {
        return crate::authorize::decide(&general(principal), action, resource);
    }
    let (Some(_), true) = (principal.user(), principal.active()) else {
        // Unauthenticated or disabled: the general answer, whatever the
        // credential.
        return crate::authorize::decide(&general(principal), action, resource);
    };
    if principal.account_kind() != AccountKind::ServiceAccount {
        return Err(refused(action, "improve_draft_not_service_account"));
    }
    if let Some((project, _)) = crate::authorize::project_of(resource) {
        if project.role.is_none() {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("action", action.name())
                .with_context("reason", "improve_draft_not_member"));
        }
    }
    if !allowed(action) {
        return Err(ForgeError::new(ErrorCode::InsufficientScope)
            .with_context("action", action.name())
            .with_context("reason", "improve_draft_not_allowed")
            .with_context("token_scopes", Scope::ImproveDraft.as_str()));
    }
    match (action, resource) {
        (Action::RepoPush, Resource::Ref { name, update, .. }) => {
            if !is_improve_branch(name) {
                return Err(refused(action, "improve_branch_only"));
            }
            // A new branch only: never moved after it was created, so a
            // merge request a person marked ready or approved never gets
            // the bot's further commits (a revision is a new branch and a
            // new draft).
            if *update != RefUpdate::Create {
                return Err(refused(action, "improve_draft_create_only"));
            }
        }
        (
            Action::MergeRequestCreate,
            Resource::MergeRequest {
                draft,
                source_branch,
                ..
            },
        ) => {
            if !*draft {
                return Err(refused(action, "draft_only"));
            }
            if !is_improve_branch(source_branch) {
                return Err(refused(action, "improve_branch_only"));
            }
        }
        _ => {}
    }
    crate::authorize::decide(&general(principal), action, resource)
}

/// The same account as the general rules decide an allowed action: a
/// token with [`AS_SCOPES`], no administrator rights, the agent flag kept.
fn general(principal: &Principal) -> Principal {
    let Some(user) = principal.user() else {
        return Principal::anonymous();
    };
    Principal::authenticated(
        user,
        false,
        principal.active(),
        Credential::Token(AS_SCOPES.to_vec()),
        principal.account_kind(),
    )
    .as_agent(principal.is_agent())
}
