//! `authorize()` — the single authorization decision point (CLAUDE.md §3-4,
//! ADR-0010). Pure: no I/O, no clock, no globals.
//!
//! Order of checks:
//! 1. public actions are allowed;
//! 2. no credentials → `FG-AUTH-0001`; disabled account → `FG-AUTH-0004`;
//! 3. project visibility → `FG-REPO-0005` (404, no existence leak) and
//!    pending deletion → `FG-REPO-0006`;
//! 4. token scope → `FG-AUTH-0002` (scopes only narrow a role);
//! 5. role / ownership → `FG-AUTH-0003` (administrators pass this step);
//! 6. ref protection → `FG-GIT-0007/0008/0009` (applies to administrators too).

use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::model::{Principal, ProjectView, RefUpdate, Resource, Role, Visibility};

/// Decide whether `principal` may perform `action` on `resource`.
/// `Ok(())` = allowed; `Err` carries the code and safe context for the log.
pub fn authorize(
    principal: &Principal,
    action: Action,
    resource: &Resource,
) -> Result<(), ForgeError> {
    let deny = |code: ErrorCode| ForgeError::new(code).with_context("action", action.name());

    if action.is_public() {
        return Ok(());
    }
    let Some(user) = principal.user else {
        return Err(deny(ErrorCode::Unauthenticated));
    };
    if !principal.active {
        return Err(deny(ErrorCode::AccountDisabled));
    }

    // Visibility before anything else that could reveal existence.
    if let Some(project) = project_of(resource) {
        if !can_see(principal, project) {
            return Err(deny(ErrorCode::NotFound).with_context("reason", "not_visible"));
        }
        if project.pending_deletion && action != Action::ProjectRestore {
            return Err(deny(ErrorCode::PendingDeletion));
        }
    }

    if let (Some(scopes), Some(required)) = (&principal.token_scopes, action.required_scope()) {
        if !scopes.iter().any(|s| s.satisfies(required)) {
            let held: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
            return Err(deny(ErrorCode::InsufficientScope)
                .with_context("required_scope", required.as_str())
                .with_context("token_scopes", held.join(",")));
        }
    }

    let admin = principal.is_admin;
    let need = |required: Role, actual: Option<Role>| -> Result<(), ForgeError> {
        if admin || actual.is_some_and(|r| r >= required) {
            Ok(())
        } else {
            Err(deny(ErrorCode::Forbidden)
                .with_context("required_role", required.as_str())
                .with_context("actual_role", actual.map_or("none", Role::as_str)))
        }
    };
    let admin_only = || -> Result<(), ForgeError> {
        if admin {
            Ok(())
        } else {
            Err(deny(ErrorCode::Forbidden).with_context("required_role", "admin"))
        }
    };
    // A resource of the wrong shape for the action is a programming error in
    // the caller; deny rather than guess.
    let wrong_resource = || {
        Err(ForgeError::new(ErrorCode::Internal)
            .with_context("action", action.name())
            .with_context("reason", "resource_does_not_match_action"))
    };

    match (action, resource) {
        (Action::Health, _) => Ok(()),

        (Action::RepoRead | Action::ProjectRead, Resource::Project(p)) => {
            need(Role::Reporter, read_role(p))
        }

        (
            Action::RepoPush,
            Resource::Ref {
                project,
                update,
                protection,
            },
        ) => {
            need(Role::Developer, project.role)?;
            let Some(protection) = protection else {
                return Ok(());
            };
            match update {
                RefUpdate::Force => Err(deny(ErrorCode::ProtectedRefForcePush)),
                RefUpdate::Delete => Err(deny(ErrorCode::ProtectedRefDeletion)),
                RefUpdate::Create | RefUpdate::FastForward => {
                    let ok = admin || project.role.is_some_and(|r| r >= protection.push_min_role);
                    if ok {
                        Ok(())
                    } else {
                        Err(deny(ErrorCode::ProtectedRefPushNotAllowed)
                            .with_context("required_role", protection.push_min_role.as_str())
                            .with_context("actual_role", project.role.map_or("none", Role::as_str)))
                    }
                }
            }
        }

        (Action::ProjectCreate, Resource::Group { role }) => need(Role::Maintainer, *role),
        (Action::ProjectCreate, Resource::Namespace { owner }) => {
            if admin || *owner == user {
                Ok(())
            } else {
                Err(deny(ErrorCode::Forbidden).with_context("reason", "foreign_namespace"))
            }
        }

        (Action::ProjectUpdate | Action::ProtectedRefManage, Resource::Project(p)) => {
            need(Role::Maintainer, p.role)
        }

        (
            Action::ProjectTransfer,
            Resource::Transfer {
                source,
                target_role,
            },
        ) => {
            need(Role::Owner, source.role)?;
            need(Role::Maintainer, *target_role)
        }

        (Action::ProjectDelete | Action::ProjectRestore, Resource::Project(p)) => {
            need(Role::Owner, p.role)
        }

        (Action::MemberManage, Resource::Membership { role, granting }) => {
            need(Role::Maintainer, *role)?;
            // Nobody grants above their own role; Owner rows need an Owner.
            if admin || role.is_some_and(|r| *granting <= r) {
                Ok(())
            } else {
                Err(deny(ErrorCode::Forbidden)
                    .with_context("reason", "grant_above_own_role")
                    .with_context("granting", granting.as_str()))
            }
        }

        // Groups are visible to every signed-in user (internal).
        (Action::GroupRead, Resource::Group { .. }) => Ok(()),
        (Action::GroupCreateTopLevel, Resource::Instance) => admin_only(),
        (Action::SubgroupCreate | Action::GroupUpdate, Resource::Group { role }) => {
            need(Role::Owner, *role)
        }

        (Action::TokenCreate | Action::TokenList, Resource::Token { owner }) => {
            if *owner == user {
                Ok(())
            } else {
                Err(deny(ErrorCode::Forbidden).with_context("reason", "foreign_token"))
            }
        }
        (Action::TokenRevoke, Resource::Token { owner }) => {
            if admin || *owner == user {
                Ok(())
            } else {
                Err(deny(ErrorCode::Forbidden).with_context("reason", "foreign_token"))
            }
        }

        (Action::UserCreate | Action::UserDisable | Action::Metrics, Resource::Instance) => {
            admin_only()
        }

        (Action::AuditRead, Resource::Instance) => admin_only(),
        (Action::AuditRead, Resource::Group { role }) => need(Role::Owner, *role),
        (Action::AuditRead, Resource::Project(p)) => need(Role::Owner, p.role),

        (
            Action::RepoRead
            | Action::RepoPush
            | Action::ProjectRead
            | Action::ProjectCreate
            | Action::ProjectUpdate
            | Action::ProjectTransfer
            | Action::ProjectDelete
            | Action::ProjectRestore
            | Action::ProtectedRefManage
            | Action::MemberManage
            | Action::GroupRead
            | Action::GroupCreateTopLevel
            | Action::SubgroupCreate
            | Action::GroupUpdate
            | Action::TokenCreate
            | Action::TokenList
            | Action::TokenRevoke
            | Action::UserCreate
            | Action::UserDisable
            | Action::AuditRead
            | Action::Metrics,
            _,
        ) => wrong_resource(),
    }
}

/// The project a resource belongs to, for the visibility check.
fn project_of(resource: &Resource) -> Option<&ProjectView> {
    match resource {
        Resource::Project(p) => Some(p),
        Resource::Ref { project, .. } => Some(project),
        Resource::Transfer { source, .. } => Some(source),
        Resource::Instance
        | Resource::Group { .. }
        | Resource::Namespace { .. }
        | Resource::Membership { .. }
        | Resource::Token { .. } => None,
    }
}

/// Members see their projects; every signed-in user sees internal ones;
/// administrators see everything.
fn can_see(principal: &Principal, p: &ProjectView) -> bool {
    principal.is_admin || p.role.is_some() || p.visibility == Visibility::Internal
}

/// The role that applies to reading: on an internal project every signed-in
/// user reads like a Reporter.
fn read_role(p: &ProjectView) -> Option<Role> {
    match p.visibility {
        Visibility::Internal => Some(p.role.map_or(Role::Reporter, |r| r.max(Role::Reporter))),
        Visibility::Private => p.role,
    }
}
