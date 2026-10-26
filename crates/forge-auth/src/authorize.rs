//! `authorize()` — the single authorization decision point (CLAUDE.md §3-4,
//! ADR-0010). Pure: no I/O, no clock, no globals.
//!
//! Order of checks:
//! 1. public actions are allowed;
//! 2. no credentials → `FG-AUTH-0001`; disabled account → `FG-AUTH-0004`;
//! 3. project visibility → `FG-REPO-0005` (404, no existence leak) and
//!    pending deletion → `FG-REPO-0006` (restore excepted);
//! 4. token scope → `FG-AUTH-0002` (scopes only narrow a role);
//! 5. role / ownership → `FG-AUTH-0003` (administrator rights pass; a token
//!    carries administrator rights only with the `admin` scope);
//! 6. pushes: reserved ref namespaces → `FG-GIT-0010`; protected refs →
//!    `FG-GIT-0007` (force) / `FG-GIT-0009` (delete) for everyone, including
//!    administrators, and `FG-GIT-0008` below the minimum push role
//!    (administrators pass the minimum role).

use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::model::{
    Credential, MembershipChange, MembershipScope, Principal, ProjectView, RefKind, RefUpdate,
    Resource, Role, Scope, Visibility,
};

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
    let Some(user) = principal.user() else {
        return Err(deny(ErrorCode::Unauthenticated));
    };
    if !principal.active() {
        return Err(deny(ErrorCode::AccountDisabled));
    }
    let admin = principal.admin_rights();

    // Visibility before anything else that could reveal existence.
    if let Some(project) = project_of(resource) {
        if !(admin || can_see(project)) {
            return Err(deny(ErrorCode::NotFound).with_context("reason", "not_visible"));
        }
        if project.pending_deletion && action != Action::ProjectRestore {
            return Err(deny(ErrorCode::PendingDeletion));
        }
    }

    if let (Credential::Token(scopes), Some(required)) =
        (principal.credential(), action.required_scope())
    {
        if !scopes.iter().any(|s| s.satisfies(required)) {
            let held: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
            return Err(deny(ErrorCode::InsufficientScope)
                .with_context("required_scope", required.as_str())
                .with_context("token_scopes", held.join(",")));
        }
    }

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
    // Another user's token is answered like a missing one (no id oracle).
    let own_token = |owner| -> Result<(), ForgeError> {
        if owner == user {
            Ok(())
        } else {
            Err(deny(ErrorCode::NotFound).with_context("reason", "foreign_token"))
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
        // Public actions returned above; kept for exhaustiveness.
        (Action::Health, _) => Ok(()),

        (Action::RepoRead | Action::ProjectRead, Resource::Project(p)) => {
            need(Role::Reporter, read_role(p))
        }

        (
            Action::RepoPush,
            Resource::Ref {
                project,
                name,
                update,
                protection,
            },
        ) => {
            need(Role::Developer, project.role)?;
            if RefKind::of(name) == RefKind::Reserved {
                return Err(deny(ErrorCode::ReservedRefNamespace));
            }
            let Some(protection) = protection else {
                return Ok(());
            };
            let protected =
                |code| deny(code).with_context("protection", protection.push_min_role.as_str());
            match update {
                RefUpdate::Force => Err(protected(ErrorCode::ProtectedRefForcePush)),
                RefUpdate::Delete => Err(protected(ErrorCode::ProtectedRefDeletion)),
                RefUpdate::Create | RefUpdate::FastForward => {
                    if admin || project.role.is_some_and(|r| r >= protection.push_min_role) {
                        Ok(())
                    } else {
                        Err(protected(ErrorCode::ProtectedRefPushNotAllowed)
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

        (Action::ProjectUpdate, Resource::Project(p)) => need(Role::Maintainer, p.role),

        (Action::ProjectVisibilityChange, Resource::VisibilityChange { project, .. }) => {
            need(Role::Owner, project.role)
        }

        (Action::ProtectedRefManage, Resource::RefProtectionChange { project, from, to }) => {
            need(Role::Maintainer, project.role)?;
            // Nobody changes a protection above their own role: a Maintainer
            // cannot lift (or set) an Owner-level protection.
            match (*from).max(*to) {
                Some(level) if !admin && project.role.is_none_or(|r| level > r) => {
                    Err(deny(ErrorCode::Forbidden)
                        .with_context("reason", "above_own_role")
                        .with_context("required_role", level.as_str())
                        .with_context("actual_role", project.role.map_or("none", Role::as_str)))
                }
                Some(_) | None => Ok(()),
            }
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

        (
            Action::MemberManage,
            Resource::Membership {
                scope,
                change,
                removes_last_owner,
            },
        ) => {
            let caller = match scope {
                MembershipScope::Project(p) => p.role,
                MembershipScope::Group { role } => *role,
            };
            need(Role::Maintainer, caller)?;
            // Nobody acts on a role above their own: neither the role being
            // granted nor the member's current role (a Maintainer cannot
            // demote or remove an Owner).
            let highest = match change {
                MembershipChange::Add(to) => *to,
                MembershipChange::Change { from, to } => (*from).max(*to),
                MembershipChange::Remove { from } => *from,
            };
            if !(admin || caller.is_some_and(|r| highest <= r)) {
                return Err(deny(ErrorCode::Forbidden)
                    .with_context("reason", "above_own_role")
                    .with_context("required_role", highest.as_str())
                    .with_context("actual_role", caller.map_or("none", Role::as_str)));
            }
            // Never leave a project or group without an Owner (administrators
            // included: transfer ownership first).
            if *removes_last_owner {
                return Err(deny(ErrorCode::Forbidden).with_context("reason", "last_owner"));
            }
            Ok(())
        }

        // Groups are visible to every signed-in user (internal).
        (Action::GroupRead, Resource::Group { .. }) => Ok(()),
        (Action::GroupCreateTopLevel, Resource::Instance) => admin_only(),
        (Action::SubgroupCreate | Action::GroupUpdate, Resource::Group { role }) => {
            need(Role::Owner, *role)
        }

        (Action::TokenCreate, Resource::TokenRequest { owner, scopes }) => {
            // A token never mints tokens: a leaked token could otherwise
            // widen its scopes and outlive its own revocation.
            if matches!(principal.credential(), Credential::Token(_)) {
                return Err(deny(ErrorCode::Forbidden).with_context("reason", "token_cannot_mint"));
            }
            own_token(*owner)?;
            // An admin-scoped token only for someone who has admin rights
            // now — otherwise it would silently gain them on a later promotion.
            if scopes.contains(&Scope::Admin) && !admin {
                return Err(
                    deny(ErrorCode::Forbidden).with_context("reason", "admin_scope_requires_admin")
                );
            }
            Ok(())
        }
        (Action::TokenList, Resource::Token { owner }) => own_token(*owner),
        (Action::TokenRevoke, Resource::Token { owner }) => {
            if admin {
                Ok(())
            } else {
                own_token(*owner)
            }
        }

        (Action::UserCreate | Action::UserDisable | Action::Metrics, Resource::Instance) => {
            admin_only()
        }

        (Action::AuditRead, Resource::Instance) => admin_only(),
        (Action::AuditRead, Resource::Group { role }) => need(Role::Owner, *role),
        (Action::AuditRead, Resource::Project(p)) => need(Role::Owner, p.role),

        // Every other combination: the resource does not fit the action.
        // Both sides are listed exhaustively so a new Action *or* a new
        // Resource variant is a compile error here until it is decided.
        (
            Action::RepoRead
            | Action::RepoPush
            | Action::ProjectRead
            | Action::ProjectCreate
            | Action::ProjectUpdate
            | Action::ProjectVisibilityChange
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
            Resource::Instance
            | Resource::Group { .. }
            | Resource::Namespace { .. }
            | Resource::Project(_)
            | Resource::Ref { .. }
            | Resource::Transfer { .. }
            | Resource::Membership { .. }
            | Resource::RefProtectionChange { .. }
            | Resource::VisibilityChange { .. }
            | Resource::Token { .. }
            | Resource::TokenRequest { .. },
        ) => wrong_resource(),
    }
}

/// The project a resource belongs to, for the visibility check.
fn project_of(resource: &Resource) -> Option<&ProjectView> {
    match resource {
        Resource::Project(p) => Some(p),
        Resource::Ref { project, .. } => Some(project),
        Resource::Transfer { source, .. } => Some(source),
        Resource::RefProtectionChange { project, .. }
        | Resource::VisibilityChange { project, .. } => Some(project),
        Resource::Membership {
            scope: MembershipScope::Project(p),
            ..
        } => Some(p),
        Resource::Membership {
            scope: MembershipScope::Group { .. },
            ..
        }
        | Resource::Instance
        | Resource::Group { .. }
        | Resource::Namespace { .. }
        | Resource::Token { .. }
        | Resource::TokenRequest { .. } => None,
    }
}

/// Members see their projects; every signed-in user sees internal ones.
/// (Administrator rights are checked by the caller.)
fn can_see(p: &ProjectView) -> bool {
    p.role.is_some() || p.visibility == Visibility::Internal
}

/// The role that applies to reading: on an internal project every signed-in
/// user reads like a Reporter.
fn read_role(p: &ProjectView) -> Option<Role> {
    match p.visibility {
        Visibility::Internal => Some(p.role.map_or(Role::Reporter, |r| r.max(Role::Reporter))),
        Visibility::Private => p.role,
    }
}
