//! `authorize()` — the single authorization decision point (CLAUDE.md §3-4,
//! ADR-0010). Pure: no I/O, no clock, no globals.
//!
//! Order of checks:
//! 1. public actions are allowed; the sign-in's admission (`SessionCreate`
//!    on `Resource::Admission`, M2 MR4, ADR-0021 §3) is decided on its
//!    facts alone → `FG-AUTH-0013` (`no_access_role`, `user_disabled`,
//!    `not_human`); so is the refresh check's `SessionContinue` (M2 MR5:
//!    active accounts only, `account_new` otherwise);
//! 2. no credentials → `FG-AUTH-0001`; disabled account → `FG-AUTH-0004`;
//!    then session-only actions (`SessionRead`, `SessionLogout`,
//!    `TokenCreate`): a token → `FG-AUTH-0002 reason=session_required`
//!    (M2 MR3, ADR-0021 §17; the own-route `SessionList` /
//!    `SessionRevoke` decide the same in their arm, MR6);
//! 3. `PathResolve` only: token scope → `FG-AUTH-0002` here, before the
//!    path is looked at (the same 403 for every path);
//!    a path that resolved to nothing (`Unresolved`, any non-public action)
//!    → `FG-REPO-0005`; project visibility (also behind a resolved path) →
//!    `FG-REPO-0005` (404, no existence leak) and pending deletion →
//!    `FG-REPO-0006` (restore excepted: it is decided on the Owner role, and a
//!    caller who can see the project but may not restore it also gets
//!    `FG-REPO-0006`); a transfer target or a moved
//!    tombstone's target the caller cannot see → `FG-REPO-0005`;
//! 4. token scope → `FG-AUTH-0002` (scopes only narrow a role);
//! 5. role / ownership → `FG-AUTH-0003` (administrator rights pass; a token
//!    carries administrator rights only with the `admin` scope); an agent's
//!    credential on an action kept for people → `FG-AUTH-0003
//!    reason=agent_token` (`ProjectVisibilityChange`, `MemberManage`,
//!    `ProtectedRefManage`, and every D5 action: `ProjectTransfer`,
//!    `GroupTransfer`, `UserRename`, `NamespaceTombstoneRelease`,
//!    `ProjectDelete`, `ProjectRestore`);
//! 6. pushes: reserved ref namespaces → `FG-GIT-0010`; protected refs →
//!    `FG-GIT-0007` (force) / `FG-GIT-0009` (delete) for everyone, including
//!    administrators, and `FG-GIT-0008` below the minimum push role
//!    (administrators pass the minimum role).

use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::model::{
    AdmissionAccount, Credential, MembershipChange, MembershipScope, NamespaceView, Principal,
    ProjectView, RefKind, RefUpdate, ResolvedVia, Resource, Role, Scope, SessionRoute,
    TombstoneRelease, TransferTarget, UnresolvedReason, Visibility,
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
    // The sign-in's admission (ADR-0021 §3): decided on the facts alone —
    // there is no principal yet (the callback passes the anonymous one).
    if let (
        true,
        Resource::Admission {
            access_role,
            account,
        },
    ) = (action.is_admission(), resource)
    {
        let decided = if action == Action::SessionContinue {
            continuation(*access_role, *account)
        } else {
            admission(*access_role, *account)
        };
        return decided
            .map_err(|reason| deny(ErrorCode::LoginNotPermitted).with_context("reason", reason));
    }
    let Some(user) = principal.user() else {
        return Err(deny(ErrorCode::Unauthenticated));
    };
    if !principal.active() {
        return Err(deny(ErrorCode::AccountDisabled));
    }
    let admin = principal.admin_rights();

    // A session-only action (ADR-0021 §17): no token, whatever its scopes,
    // before anything about the resource is decided.
    if action.is_session_only() {
        if let Credential::Token(scopes) = principal.credential() {
            let held: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
            return Err(deny(ErrorCode::InsufficientScope)
                .with_context("reason", "session_required")
                .with_context("token_scopes", held.join(",")));
        }
    }

    let check_scope = || -> Result<(), ForgeError> {
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
        Ok(())
    };

    // Resolving a path: the scope first, so a wrong-scope token gets the
    // same 403 for every path (absent, invisible or visible alike).
    if action == Action::PathResolve {
        check_scope()?;
    }

    // Nothing at the path: the same 404 for every non-public action, as an
    // invisible project gets (no existence oracle through any route).
    let unresolved = |r: UnresolvedReason| -> Result<(), ForgeError> {
        Err(deny(ErrorCode::NotFound).with_context("reason", r.as_str()))
    };
    if let Resource::Unresolved(reason) = resource {
        return unresolved(*reason);
    }

    // Visibility before anything else that could reveal existence.
    if let Some((project, via)) = project_of(resource) {
        if !(admin || can_see(project)) {
            let reason = match via {
                ResolvedVia::Live => "not_visible",
                ResolvedVia::Moved => "tombstone_not_visible",
            };
            return Err(deny(ErrorCode::NotFound).with_context("reason", reason));
        }
        if project.pending_deletion && action != Action::ProjectRestore {
            return Err(deny(ErrorCode::PendingDeletion));
        }
    }
    // The other side of a move, or a moved tombstone's target, the caller
    // cannot see (or that does not exist): the same 404 (ADR-0015).
    // Administrators too: the loader shows them every target, so for them
    // `visible: false` means "absent" — never a pass to a missing parent.
    // Before the scope and agent checks, so every caller gets the same 404.
    if let Some(reason) = hidden_target(resource, admin) {
        return Err(deny(ErrorCode::NotFound).with_context("reason", reason));
    }

    check_scope()?;

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
    // Actions kept for people (ADR-0017 amendments MR7a, MR8, D5): an
    // agent's credential is refused whatever its role, administrators
    // included. Checked after visibility and scope, before roles.
    let people_only = || -> Result<(), ForgeError> {
        if principal.is_agent() {
            Err(deny(ErrorCode::Forbidden).with_context("reason", "agent_token"))
        } else {
            Ok(())
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
        (
            Action::Health | Action::WebShell | Action::WebLogin | Action::OidcBackchannelLogout,
            _,
        ) => Ok(()),

        // Visibility (can see, not a read role: a Guest on a private project
        // resolves it) and pending deletion were decided above; groups and
        // personal namespaces are visible to every signed-in user.
        (Action::PathResolve, Resource::Resolved { .. }) => Ok(()),
        // Returned above for every non-public action; kept for exhaustiveness.
        (_, Resource::Unresolved(reason)) => unresolved(*reason),

        (Action::RepoRead | Action::RepoBrowse | Action::ProjectRead, Resource::Project(p)) => {
            need(Role::Reporter, read_role(p))
        }

        // Before receive-pack starts (no ref is known yet): may this caller
        // push to the project at all? Stops non-developers from uploading
        // packs; each ref update is decided again below.
        (Action::RepoPush, Resource::Project(p)) => need(Role::Developer, p.role),

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
                |code| deny(code).with_context("protection", protection.push_min_role().as_str());
            match update {
                RefUpdate::Force => Err(protected(ErrorCode::ProtectedRefForcePush)),
                RefUpdate::Delete => Err(protected(ErrorCode::ProtectedRefDeletion)),
                RefUpdate::Create | RefUpdate::FastForward => {
                    if admin
                        || project
                            .role
                            .is_some_and(|r| r >= protection.push_min_role())
                    {
                        Ok(())
                    } else {
                        Err(protected(ErrorCode::ProtectedRefPushNotAllowed)
                            .with_context("required_role", protection.push_min_role().as_str())
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
            // Exposing a project to every signed-in user is a person's
            // decision (ADR-0017 amendment, M1 phase 8b MR7a): an agent's
            // credential is refused whatever its role, administrators
            // included.
            people_only()?;
            need(Role::Owner, project.role)
        }

        // The direct members and the protection rules are read like the
        // project (Reporter; every signed-in user on an internal project).
        (Action::MemberList | Action::ProtectedRefList, Resource::Project(p)) => {
            need(Role::Reporter, read_role(p))
        }
        // A group's members, like the group, are visible to every signed-in user.
        (Action::MemberList, Resource::Group { .. }) => Ok(()),

        (Action::ProtectedRefManage, Resource::RefProtectionChange { project, from, to }) => {
            // Lifting a protection lets the next push through: a person's
            // decision (ADR-0017 amendment, M1 phase 8b MR8), whatever the
            // agent's role.
            people_only()?;
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

        // A project moves (or is renamed) with Owner on it and the right to
        // create in the target: Maintainer on a group, or the personal
        // namespace's own user. Never to the top level — a project lives
        // under a namespace (ADR-0015), administrators included.
        (Action::ProjectTransfer, Resource::Transfer { source, target }) => {
            people_only()?;
            need(Role::Owner, source.role)?;
            match *target {
                TransferTarget::Group { role, .. } => need(Role::Maintainer, role),
                TransferTarget::PersonalNamespace { owner } => {
                    if admin || owner == user {
                        Ok(())
                    } else {
                        Err(deny(ErrorCode::Forbidden).with_context("reason", "foreign_namespace"))
                    }
                }
                // A rename: Owner on the project is all it takes (a project
                // is never at the top level; that shape is refused).
                TransferTarget::CurrentParent { top_level: false } => Ok(()),
                TransferTarget::TopLevel | TransferTarget::CurrentParent { top_level: true } => {
                    Err(deny(ErrorCode::Forbidden).with_context("reason", "project_to_top_level"))
                }
            }
        }

        // A group moves (or is renamed) with Owner on both sides: every
        // member of the target inherits a role in it. To the top level only
        // by an administrator (no detour around `GroupCreateTopLevel`);
        // never into a personal namespace.
        (
            Action::GroupTransfer,
            Resource::GroupTransfer {
                source_role,
                target,
            },
        ) => {
            people_only()?;
            need(Role::Owner, *source_role)?;
            match *target {
                TransferTarget::Group { role, .. } => need(Role::Owner, role),
                // A rename below the same group: Owner of the group is
                // enough (a subgroup's Owner needs no role on the parent);
                // at the top level it is an administrator's.
                TransferTarget::CurrentParent { top_level: false } => Ok(()),
                TransferTarget::TopLevel | TransferTarget::CurrentParent { top_level: true } => {
                    admin_only()
                }
                TransferTarget::PersonalNamespace { .. } => Err(deny(ErrorCode::Forbidden)
                    .with_context("reason", "group_to_personal_namespace")),
            }
        }

        // Usernames and personal namespace paths: administrators only
        // (ADR-0015; the user themself included).
        (Action::UserRename, Resource::UserRename { .. }) => {
            people_only()?;
            admin_only()
        }

        (Action::NamespaceTombstoneRelease, Resource::Tombstone(release)) => {
            people_only()?;
            match *release {
                // The target's current Owner takes its old path back.
                TombstoneRelease::Moved {
                    target_owner_role, ..
                } => need(Role::Owner, target_owner_role),
                TombstoneRelease::Deleted {
                    recorded_owner,
                    caller_parent_role,
                    has_parent,
                } => {
                    // A non-administrator without a role in the parent
                    // (always so without a parent) got the 404 above.
                    if admin {
                        return Ok(());
                    }
                    // The recorded owner, still Owner of the parent (an
                    // inactive one was refused above with FG-AUTH-0004).
                    if has_parent
                        && recorded_owner == Some(user)
                        && caller_parent_role.is_some_and(|r| r >= Role::Owner)
                    {
                        Ok(())
                    } else {
                        Err(deny(ErrorCode::Forbidden)
                            .with_context("reason", "not_recorded_owner")
                            .with_context(
                                "actual_role",
                                caller_parent_role.map_or("none", Role::as_str),
                            ))
                    }
                }
            }
        }

        // Deleting and restoring a project is a person's decision (D5).
        (Action::ProjectDelete | Action::ProjectRestore, Resource::Project(p)) => {
            people_only()?;
            // A caller who can see a project pending deletion but may not
            // restore it gets what every other route gives there, 410
            // (FG-REPO-0006), not a 403 that only restore would answer.
            need(Role::Owner, p.role).map_err(|e| {
                if action == Action::ProjectRestore && p.pending_deletion {
                    deny(ErrorCode::PendingDeletion)
                } else {
                    e
                }
            })
        }

        (
            Action::MemberManage,
            Resource::Membership {
                scope,
                change,
                removes_last_owner,
            },
        ) => {
            // Who holds a role is a person's decision (ADR-0017 amendment,
            // M1 phase 8b MR8): an agent could otherwise raise itself or
            // another agent's account.
            people_only()?;
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
                MembershipChange::Missing { to } => to.unwrap_or(Role::Guest),
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
            // widen its scopes and outlive its own revocation. Refused
            // above as session-only (FG-AUTH-0002, M2 MR6); kept as the
            // backstop should the action ever leave that list.
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

        (Action::UserCreate | Action::UserAdminList | Action::Metrics, Resource::Instance) => {
            admin_only()
        }
        // Disabling a person is a person's decision (M2 MR6): an agent's
        // credential is refused, administrators' agents included. The
        // administration guard (last administrator, last Owner) is the
        // command's, always enforced over HTTP.
        (Action::UserDisable, Resource::Instance) => {
            people_only()?;
            admin_only()
        }
        // Finding users by name (the member picker): every active signed-in
        // user (the scope was checked above); names only.
        (Action::UserSearch, Resource::Instance) => Ok(()),

        // Web sessions (M2 MR6, ADR-0021 §17).
        (Action::SessionList | Action::SessionRevoke, Resource::Session { owner, route }) => {
            match route {
                // The user's own route: sessions only (a token never sees
                // or ends a browser session), and another user's session is
                // not found — administrators included (they use the
                // administration route).
                SessionRoute::Own => {
                    if let Credential::Token(scopes) = principal.credential() {
                        let held: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
                        return Err(deny(ErrorCode::InsufficientScope)
                            .with_context("reason", "session_required")
                            .with_context("token_scopes", held.join(",")));
                    }
                    if *owner == user {
                        Ok(())
                    } else {
                        Err(deny(ErrorCode::NotFound).with_context("reason", "foreign_session"))
                    }
                }
                // The administration's route: administrator rights; ending
                // someone's sessions is a person's decision.
                SessionRoute::Admin => {
                    if action == Action::SessionRevoke {
                        people_only()?;
                    }
                    admin_only()
                }
            }
        }

        // Every active signed-in user reads their own account (the scope
        // was checked above); the handler shows the principal's own data.
        (Action::UserSelfRead, Resource::Instance) => Ok(()),

        // Every active user reads their own session (a token was refused
        // above); the handler shows the principal's own session only.
        (Action::SessionRead, Resource::Instance) => Ok(()),

        // Every active user ends their own session (a token was refused
        // above); the handler revokes the request's own session only.
        (Action::SessionLogout, Resource::Instance) => Ok(()),

        // Listing is open to every active signed-in user (the scope was
        // checked above); what a list shows is decided row by row with
        // `ProjectRead` / `GroupRead` (M1 phase 8b MR7b, never in SQL).
        (Action::ProjectList | Action::GroupList, Resource::Instance) => Ok(()),

        (Action::AuditRead, Resource::Instance) => admin_only(),
        (Action::AuditRead, Resource::Group { role }) => need(Role::Owner, *role),
        (Action::AuditRead, Resource::Project(p)) => need(Role::Owner, p.role),

        // Every other combination: the resource does not fit the action.
        // Both sides are listed exhaustively so a new Action *or* a new
        // Resource variant is a compile error here until it is decided.
        (
            Action::RepoRead
            | Action::RepoPush
            | Action::RepoBrowse
            | Action::ProjectRead
            | Action::ProjectList
            | Action::ProjectCreate
            | Action::ProjectUpdate
            | Action::ProjectVisibilityChange
            | Action::ProjectTransfer
            | Action::ProjectDelete
            | Action::ProjectRestore
            | Action::ProtectedRefManage
            | Action::MemberManage
            | Action::MemberList
            | Action::ProtectedRefList
            | Action::GroupRead
            | Action::GroupList
            | Action::GroupCreateTopLevel
            | Action::SubgroupCreate
            | Action::GroupUpdate
            | Action::GroupTransfer
            | Action::NamespaceTombstoneRelease
            | Action::UserRename
            | Action::TokenCreate
            | Action::TokenList
            | Action::TokenRevoke
            | Action::UserCreate
            | Action::UserDisable
            | Action::UserSearch
            | Action::UserAdminList
            | Action::AuditRead
            | Action::UserSelfRead
            | Action::SessionRead
            | Action::SessionList
            | Action::SessionRevoke
            | Action::SessionCreate
            | Action::SessionContinue
            | Action::SessionLogout
            | Action::PathResolve
            | Action::Metrics,
            Resource::Instance
            | Resource::Group { .. }
            | Resource::Namespace { .. }
            | Resource::Project(_)
            | Resource::Ref { .. }
            | Resource::Transfer { .. }
            | Resource::GroupTransfer { .. }
            | Resource::UserRename { .. }
            | Resource::Tombstone(_)
            | Resource::Membership { .. }
            | Resource::RefProtectionChange { .. }
            | Resource::VisibilityChange { .. }
            | Resource::Token { .. }
            | Resource::Session { .. }
            | Resource::TokenRequest { .. }
            | Resource::Admission { .. }
            | Resource::Resolved { .. },
        ) => wrong_resource(),
    }
}

/// `SessionCreate` (ADR-0021 §3, §17): the access role, and a new or active
/// human account. The denial's log `reason` otherwise.
fn admission(access_role: bool, account: AdmissionAccount) -> Result<(), &'static str> {
    match account {
        AdmissionAccount::Disabled => Err("user_disabled"),
        AdmissionAccount::NotHuman => Err("not_human"),
        AdmissionAccount::New | AdmissionAccount::Active if !access_role => Err("no_access_role"),
        AdmissionAccount::New | AdmissionAccount::Active => Ok(()),
    }
}

/// `SessionContinue` (ADR-0021 §7, §17): the access role, and an active
/// human account (`New` cannot happen to a session's account and is
/// refused). The denial's log `reason` otherwise.
fn continuation(access_role: bool, account: AdmissionAccount) -> Result<(), &'static str> {
    match account {
        AdmissionAccount::Disabled => Err("user_disabled"),
        AdmissionAccount::NotHuman => Err("not_human"),
        AdmissionAccount::New => Err("account_new"),
        AdmissionAccount::Active if !access_role => Err("no_access_role"),
        AdmissionAccount::Active => Ok(()),
    }
}

/// The project a resource belongs to, for the visibility check, and how it
/// was reached (only a resolved path can arrive through a tombstone).
fn project_of(resource: &Resource) -> Option<(&ProjectView, ResolvedVia)> {
    let live = |p| Some((p, ResolvedVia::Live));
    match resource {
        Resource::Project(p) => live(p),
        Resource::Ref { project, .. } => live(project),
        Resource::Transfer { source, .. } => live(source),
        Resource::RefProtectionChange { project, .. }
        | Resource::VisibilityChange { project, .. } => live(project),
        Resource::Membership {
            scope: MembershipScope::Project(p),
            ..
        } => live(p),
        Resource::Resolved {
            via,
            view: NamespaceView::Project(p),
        } => Some((p, *via)),
        // Groups and personal namespaces are visible to every signed-in user.
        Resource::Resolved {
            view: NamespaceView::User | NamespaceView::Group { .. },
            ..
        }
        | Resource::Unresolved(_) => None,
        Resource::Membership {
            scope: MembershipScope::Group { .. },
            ..
        }
        | Resource::Instance
        | Resource::Group { .. }
        | Resource::Namespace { .. }
        | Resource::Token { .. }
        | Resource::Session { .. }
        | Resource::TokenRequest { .. }
        | Resource::Admission { .. }
        | Resource::GroupTransfer { .. }
        | Resource::UserRename { .. }
        | Resource::Tombstone(_) => None,
    }
}

/// The log `reason` when the far side of a resource is hidden from the
/// caller: a transfer's target group, or a moved tombstone's target.
fn hidden_target(resource: &Resource, admin: bool) -> Option<&'static str> {
    if let Resource::Transfer { target, .. } | Resource::GroupTransfer { target, .. } = resource {
        if matches!(target, TransferTarget::Group { visible: false, .. }) {
            return Some("target_not_visible");
        }
    }
    match resource {
        Resource::Tombstone(TombstoneRelease::Moved {
            target_visible: false,
            ..
        }) => Some("tombstone_not_visible"),
        // A deleted tombstone is seen through the parent: no role there (or
        // no parent at all) is the same 404 as no tombstone, for every
        // non-administrator (no probing of deleted paths).
        Resource::Tombstone(TombstoneRelease::Deleted {
            caller_parent_role,
            has_parent,
            ..
        }) if !admin && (!has_parent || caller_parent_role.is_none()) => {
            Some("tombstone_not_visible")
        }
        Resource::Tombstone(TombstoneRelease::Moved { .. } | TombstoneRelease::Deleted { .. })
        | Resource::Instance
        | Resource::Group { .. }
        | Resource::Namespace { .. }
        | Resource::Project(_)
        | Resource::Ref { .. }
        | Resource::Transfer { .. }
        | Resource::GroupTransfer { .. }
        | Resource::UserRename { .. }
        | Resource::Membership { .. }
        | Resource::RefProtectionChange { .. }
        | Resource::VisibilityChange { .. }
        | Resource::Token { .. }
        | Resource::Session { .. }
        | Resource::TokenRequest { .. }
        | Resource::Admission { .. }
        | Resource::Unresolved(_)
        | Resource::Resolved { .. } => None,
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
