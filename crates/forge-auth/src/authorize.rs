//! `authorize()` — the single authorization decision point (CLAUDE.md §3-4,
//! ADR-0010). Pure: no I/O, no clock, no globals.
//!
//! Order of checks:
//! 0. an act-as request (`act_as.rs`) and a CI runner or job (`job.rs`,
//!    M4 C0, ADR-0029) are decided in their own arms, first;
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
//! 6. merge requests (M3 R1a, ADR-0024 §12): both branches `refs/heads/*`
//!    and different, or `FG-AUTH-0003` (`not_a_branch`,
//!    `source_equals_target`) in every arm; a merged one is never closed
//!    or reopened, an edit of the target or the draft flag needs it open
//!    (`merged`, `not_open`), another's draft flag needs Maintainer
//!    (`foreign_draft`);
//!    discussions (M3 R3): commenting needs Reporter by membership;
//!    resolving is people only (`agent_token`, `service_account`, `bot`
//!    — the account kind joined the actions kept for people here), the
//!    thread's starter or a Developer who is not the merge request's
//!    author (`author_foreign_discussion`, administrators included);
//! 7. service accounts (M3 S0): see only the projects they are members of
//!    (an internal project is no exception: `FG-REPO-0005
//!    reason=service_account_not_member`) and create nothing in their own
//!    personal namespace (`non_person_namespace`, a bot too); the
//!    administration routes of service accounts need administrator rights,
//!    and a target of another kind is not found (`not_service_account`);
//!    issuing their tokens is a person's, in a web session, never with
//!    `admin` and never for a disabled account (`target_disabled`);
//! 8. integrations (M3 I1a): creating and changing one is a person's,
//!    an administrator's web session; reading them administrators' (people
//!    only); binding a project a Maintainer's (people only);
//! 9. pushes: reserved ref namespaces → `FG-GIT-0010`; protected refs →
//!    `FG-GIT-0007` (force) / `FG-GIT-0009` (delete) for everyone, including
//!    administrators, `FG-GIT-0017` (a create or fast-forward of a branch
//!    whose merge rule forbids direct pushes, M3 R6) for everyone too, and
//!    `FG-GIT-0008` below the minimum push role (administrators pass the
//!    minimum role).
//!    A whole copy of a repository (`RepoRead` on `RepoCopy`: clone /
//!    fetch, an archive; plan 06 D-5 safety net) is read like the project,
//!    then refused to an AI agent audience unless the default tip was
//!    measured open → `FG-AUTH-0022` (`tip_confidential`,
//!    `default_branch_unborn`, `tip_undecided`, `tip_not_measured`);
//! 10. an act-as confirmation (`ActAsConfirmView`, `ActAsAssertionIssue`,
//!     ADR-0025 decision of 2026-11-10): every refusal but `FG-AUTH-0001` /
//!     `FG-AUTH-0004` becomes `FG-REPO-0005` (the invisible project's 404),
//!     the cause kept in the log as `refused_code` and its context.

use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use forge_core::merge_conditions::MAX_CODEOWNERS_SUSPENSION_MINUTES;

use crate::model::{
    AccountKind, AdmissionAccount, CodeownersToggle, CopyMark, Credential, MembershipChange,
    MembershipScope, MergeRequestState, NamespaceView, Principal, ProjectView, RefKind, RefUpdate,
    ResolvedVia, Resource, Role, Scope, ServiceAccountView, TombstoneRelease, TransferTarget,
    UnresolvedReason, Visibility,
};

/// Decide whether `principal` may perform `action` on `resource`.
/// `Ok(())` = allowed; `Err` carries the code and safe context for the log.
pub fn authorize(
    principal: &Principal,
    action: Action,
    resource: &Resource,
) -> Result<(), ForgeError> {
    let decided = decide(principal, action, resource);
    if matches!(
        action,
        Action::ActAsConfirmView | Action::ActAsAssertionIssue
    ) {
        return decided.map_err(confirmation_refused);
    }
    decided
}

/// ADR-0025 (decision of 2026-11-10): an act-as confirmation answers
/// whoever may not press it — no right for the intent, a token, an act-as
/// request, a service account or a bot, a pending deletion, a disabled
/// integration, a binding state that does not fit the intent — with the
/// invisible project's `FG-REPO-0005`, so nobody learns whether an
/// integration exists or what the binding state is. The log keeps the
/// cause: the refusal's own code as `refused_code` and its context.
/// No credential (`FG-AUTH-0001`) and a disabled account (`FG-AUTH-0004`)
/// say nothing about the confirmation and stay as they are; so does a
/// defect (`FG-SYS-0001`, a resource of the wrong shape).
fn confirmation_refused(refusal: ForgeError) -> ForgeError {
    let code = refusal.code();
    // `Internal` (a resource of the wrong shape) is a defect, not a refusal.
    if matches!(
        code,
        ErrorCode::NotFound
            | ErrorCode::Unauthenticated
            | ErrorCode::AccountDisabled
            | ErrorCode::Internal
    ) {
        return refusal;
    }
    refusal.context().iter().fold(
        ForgeError::new(ErrorCode::NotFound).with_context("refused_code", code.id()),
        |hidden, (key, value)| hidden.with_context(key, value),
    )
}

/// The decision itself (see [`authorize`]).
fn decide(principal: &Principal, action: Action, resource: &Resource) -> Result<(), ForgeError> {
    let deny = |code: ErrorCode| ForgeError::new(code).with_context("action", action.name());

    // Acting for a person (M3 S2, ADR-0025): its own arm, before anything
    // else (a public action included: no act-as principal reaches one).
    if let Credential::ActAs(credential) = principal.credential() {
        return crate::act_as::decide(principal, credential, action, resource);
    }
    // A CI runner or job (M4 C0, ADR-0029 §2): its own arm, before
    // anything else, closed by default (`Action::ci_rule`).
    if let Credential::Runner(_) | Credential::Job(_) = principal.credential() {
        return crate::job::decide(principal, action, resource);
    }
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
    let kind = principal.account_kind();
    // The role that applies to reading for this caller: on an internal
    // project every signed-in person reads like a Reporter, a service
    // account only by membership (M3 S0).
    let read_role = |p: &ProjectView| read_role(kind, p);

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
        if !(admin || can_see(kind, project)) {
            let reason = match via {
                // An internal project a person would see (M3 S0).
                ResolvedVia::Live | ResolvedVia::Moved if can_see(AccountKind::Human, project) => {
                    "service_account_not_member"
                }
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
    // Another user's notification settings are answered like missing ones.
    let own_preferences = |owner| -> Result<(), ForgeError> {
        if owner == user {
            Ok(())
        } else {
            Err(deny(ErrorCode::NotFound)
                .with_context("reason", "foreign_notification_preferences"))
        }
    };
    // Actions kept for people (ADR-0017 amendments MR7a, MR8, D5): an
    // agent's credential is refused whatever its role, administrators
    // included. Checked after visibility and scope, before roles.
    // Since M3 R3 (ADR-0024 §8.4) also a service account's or a bot's
    // credential (`reason=service_account|bot`, from `users.kind`).
    let people_only = || -> Result<(), ForgeError> {
        if principal.is_agent() {
            return Err(deny(ErrorCode::Forbidden).with_context("reason", "agent_token"));
        }
        match principal.account_kind() {
            AccountKind::Human => Ok(()),
            AccountKind::ServiceAccount => {
                Err(deny(ErrorCode::Forbidden).with_context("reason", "service_account"))
            }
            AccountKind::Bot => Err(deny(ErrorCode::Forbidden).with_context("reason", "bot")),
        }
    };
    // A refusal on the request's shape or the target's state (merge
    // requests, ADR-0024 §12): 403 with the log `reason`.
    let refused = |reason: &'static str| deny(ErrorCode::Forbidden).with_context("reason", reason);
    // Who acts on an existing merge request: its author (who still reads
    // the project) or Developer and above (ADR-0024 §12).
    let author_or_developer = |own: bool, project: &ProjectView| -> Result<(), ForgeError> {
        if own {
            need(Role::Reporter, read_role(project))
        } else {
            need(Role::Developer, project.role)
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

        // `SpecRefRead` (M3 X1) and `SpecRead` (M3 X4): the project by the
        // route, and every referenced project again by `prove_spec_ref`.
        (
            Action::RepoRead
            | Action::RepoBrowse
            | Action::SpecRefRead
            | Action::SpecRead
            | Action::ContextRead
            | Action::ProjectRead,
            Resource::Project(p),
        ) => need(Role::Reporter, read_role(p)),

        // A whole copy of the repository (clone / fetch, an archive; plan
        // 06 D-5, ADR-0026 decision of 2026-11-10): read like the project,
        // then an AI agent audience (an `is_agent` credential, a service
        // account, a bot) only when the default tip was measured open. Push
        // is not a copy and stays `RepoPush`'s.
        (Action::RepoRead, Resource::RepoCopy { project, mark }) => {
            need(Role::Reporter, read_role(project))?;
            if !principal.is_agent_audience() || *mark == CopyMark::Open {
                return Ok(());
            }
            Err(deny(ErrorCode::RepositoryCopyWithheld)
                .with_context("reason", mark.as_str())
                .with_context("audience", audience_of(principal)))
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
                // M3 R6 (ADR-0024 §4.3, §12): the branch changes through
                // merge requests only — for every caller, administrators
                // included (like a force push or a deletion). The merge
                // path (`MergeRequestMerge`) never reads it.
                RefUpdate::Create | RefUpdate::FastForward
                    if protection.direct_push_forbidden() =>
                {
                    Err(protected(ErrorCode::ProtectedRefDirectPushForbidden)
                        .with_context("reason", "direct_push_forbidden"))
                }
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
        (Action::ProjectCreate, Resource::Namespace { owner, owner_kind }) => {
            // M3 S0 (and its review): the personal namespace of a service
            // account or a bot holds no projects, whoever creates them —
            // administrators included (its owner would be the project's
            // implicit Owner).
            if *owner_kind != AccountKind::Human {
                return Err(refused("non_person_namespace"));
            }
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
                TransferTarget::PersonalNamespace { owner, owner_kind } => {
                    // Not into a non-person's namespace (S0 review).
                    if owner_kind != AccountKind::Human {
                        return Err(refused("non_person_namespace"));
                    }
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
        // The user's own route (M2 MR6, plan 10 §5): another user's token
        // is not found for everyone, administrators included (an
        // administration route for others' tokens is not built yet).
        (Action::TokenRevoke, Resource::Token { owner }) => own_token(*owner),
        // M3 N1: a person's own mail settings, for nobody else — an
        // administrator included (support reads the delivery records).
        // M3 U4: their own in-app notifications likewise (the resource is
        // the person's notifications; each listed item is decided again on
        // `MergeRequestRead` by forge-notify).
        (
            Action::NotificationPreferencesRead
            | Action::NotificationPreferencesWrite
            | Action::NotificationInboxRead
            | Action::NotificationInboxUpdate,
            Resource::NotificationPreferences { owner },
        ) => own_preferences(*owner),
        // Plan 15 MR 6: the merge requests of a person's own work, for
        // nobody else — an administrator included. Each listed row is
        // decided again on `MergeRequestRead` (an approvable one also on
        // `MergeRequestApprove`) by forge-review.
        (Action::UserMergeRequestList, Resource::UserMergeRequests { owner }) => {
            if *owner == user {
                Ok(())
            } else {
                Err(deny(ErrorCode::NotFound).with_context("reason", "foreign_merge_request_list"))
            }
        }
        // Plan 15 MR 8: the recent events of the projects a person is
        // involved in, for nobody else — an administrator included — and
        // for people only (an agent's token, a service account and a bot
        // are refused; act-as and CI credentials never reach here). Each
        // listed event is decided again below, per project and class.
        (Action::ActivityRead, Resource::UserActivity { owner }) => {
            if *owner != user {
                return Err(deny(ErrorCode::NotFound).with_context("reason", "foreign_activity"));
            }
            people_only()
        }
        // One project's events of one class for the activity list
        // (visibility and pending deletion were decided above:
        // FG-REPO-0005 / FG-REPO-0006): people only, the integration class
        // only (never the maintainer, deployment or admin class, whatever
        // the role), Reporter and above by role — an internal project the
        // person merely reads is not one they are involved in. As in every
        // `need`, administrator rights pass the role: an administrator sees
        // every project they hold a membership in, a Guest one included.
        (Action::ActivityRead, Resource::ProjectEvents { project, class }) => {
            people_only()?;
            match class {
                forge_core::VisibilityClass::Integration => {}
                forge_core::VisibilityClass::Maintainer
                | forge_core::VisibilityClass::Deployment
                | forge_core::VisibilityClass::Admin => {
                    return Err(
                        refused("class_not_in_activity").with_context("class", class.as_str())
                    )
                }
            }
            need(Role::Reporter, project.role)
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

        // Web sessions (M2 MR6, ADR-0021 §17). The user's own route is
        // session-only (a token was refused above, before the lookup), and
        // another user's session is not found — administrators included
        // (they use the administration's route).
        (Action::SessionList | Action::SessionRevoke, Resource::Session { owner }) => {
            if *owner == user {
                Ok(())
            } else {
                Err(deny(ErrorCode::NotFound).with_context("reason", "foreign_session"))
            }
        }
        // The administration's route: administrator rights; ending
        // someone's sessions is a person's decision.
        (Action::AdminSessionList, Resource::Session { .. }) => admin_only(),
        (Action::AdminSessionRevoke, Resource::Session { .. }) => {
            people_only()?;
            admin_only()
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

        // --- merge requests (M3 R1a, ADR-0024 §12) ---
        // Listing and reading as the repository browse API.
        (Action::MergeRequestList, Resource::Project(p)) => need(Role::Reporter, read_role(p)),
        (
            Action::MergeRequestRead,
            Resource::MergeRequest {
                project,
                source_branch,
                target_branch,
                ..
            },
        ) => {
            need(Role::Reporter, read_role(project))?;
            branches(source_branch, target_branch).map_err(refused)
        }
        // Opening one: Developer, like a push (not the source branch's
        // protection: opening is not a push, plan 11 §11.7), for oneself.
        (
            Action::MergeRequestCreate,
            Resource::MergeRequest {
                project,
                author,
                source_branch,
                target_branch,
                ..
            },
        ) => {
            need(Role::Developer, project.role)?;
            if *author != user {
                return Err(refused("foreign_author"));
            }
            branches(source_branch, target_branch).map_err(refused)
        }
        (
            Action::MergeRequestUpdate,
            Resource::MergeRequest {
                project,
                author,
                state,
                draft,
                source_branch,
                target_branch,
                edit,
                target_protection: _,
            },
        ) => {
            let own = *author == user;
            author_or_developer(own, project)?;
            let target = edit.target_branch.as_ref().unwrap_or(target_branch);
            branches(source_branch, target).map_err(refused)?;
            let toggles = edit.draft.is_some_and(|d| d != *draft);
            let changes_branch = target != target_branch;
            if (toggles || changes_branch) && *state != MergeRequestState::Open {
                return Err(refused("not_open"));
            }
            // Another's draft flag: the author or Maintainer (ADR-0024 §12,
            // plan 11 §4.3 "他人のドラフトの変更").
            if toggles && !own {
                need(Role::Maintainer, project.role)
                    .map_err(|e| e.with_context("reason", "foreign_draft"))?;
            }
            Ok(())
        }
        (
            Action::MergeRequestClose | Action::MergeRequestReopen,
            Resource::MergeRequest {
                project,
                author,
                state,
                source_branch,
                target_branch,
                ..
            },
        ) => {
            author_or_developer(*author == user, project)?;
            branches(source_branch, target_branch).map_err(refused)?;
            // `merged` is final (ADR-0024 §6): never closed, never reopened.
            if *state == MergeRequestState::Merged {
                return Err(refused("merged"));
            }
            Ok(())
        }

        // Removing version refs (secret removal, M3 R1b, ADR-0024 §2): a
        // person (a token was refused above as session-only; agents and
        // service accounts hold only tokens) with administrator rights.
        // Whether the merge request's state allows it is the command's
        // (FG-REVIEW-0002), not a permission.
        (
            Action::MergeRequestVersionRefsPurge,
            Resource::MergeRequest {
                source_branch,
                target_branch,
                ..
            },
        ) => {
            people_only()?;
            admin_only()?;
            branches(source_branch, target_branch).map_err(refused)
        }

        // Merging (M3 R4, ADR-0024 §4.3, §12): people only; the target's
        // minimum push role (Developer when unprotected), administrators
        // too. The direct-push ban of a protected branch (R6) is not read:
        // merge requests are how such a branch changes. Readiness (open,
        // draft, conflicts, …) is `evaluate_merge()`'s, never decided here.
        (
            Action::MergeRequestMerge,
            Resource::MergeRequest {
                project,
                source_branch,
                target_branch,
                target_protection,
                ..
            },
        ) => {
            people_only()?;
            branches(source_branch, target_branch).map_err(refused)?;
            let required = target_protection
                .map_or(Role::Developer, |p| p.push_min_role().max(Role::Developer));
            need(required, project.role).map_err(|e| match target_protection {
                Some(p) => e.with_context("protection", p.push_min_role().as_str()),
                None => e,
            })
        }

        // Approving / unapproving (M3 R5, ADR-0024 §8.3, §8.4, §12): people
        // only; never the author — administrators included, before any role
        // (FG-REVIEW-0013); Developer and above by membership. Whether the
        // approval counts is evaluate_merge()'s at evaluation time.
        (
            Action::MergeRequestApprove | Action::MergeRequestUnapprove,
            Resource::MergeRequest {
                project,
                author,
                source_branch,
                target_branch,
                ..
            },
        ) => {
            people_only()?;
            branches(source_branch, target_branch).map_err(refused)?;
            if *author == user {
                return Err(
                    deny(ErrorCode::SelfApprovalRefused).with_context("reason", "self_approval")
                );
            }
            need(Role::Developer, project.role)
        }
        // Approval rules (M3 R5, ADR-0024 §8.6): people only, Maintainer.
        (Action::ApprovalRuleManage, Resource::Project(p)) => {
            people_only()?;
            need(Role::Maintainer, p.role)
        }
        // Merge settings (M3 R6, §4.1, §8.2, §8.3): people only, Maintainer;
        // a change that loosens the approvals (committers' approvals
        // counted, approvals carried on push) also needs a role at or above
        // every protection of the project, like suspending CODEOWNERS
        // (R6 review). Tightening is any Maintainer's.
        (
            Action::MergeSettingsManage,
            Resource::MergeSettingsChange {
                project,
                highest_protection,
                loosens,
            },
        ) => {
            people_only()?;
            need(Role::Maintainer, project.role)?;
            match highest_protection {
                Some(level) if *loosens && !admin && project.role.is_none_or(|r| *level > r) => {
                    Err(deny(ErrorCode::Forbidden)
                        .with_context("reason", "above_own_role")
                        .with_context("required_role", level.as_str())
                        .with_context("actual_role", project.role.map_or("none", Role::as_str)))
                }
                Some(_) | None => Ok(()),
            }
        }
        // CODEOWNERS enforcement (M3 R6, ADR-0024 §9): people only,
        // Maintainer. A suspension needs a role at or above every
        // protection of the project (a Maintainer cannot drop the owners'
        // check where an Owner-level branch exists; administrators pass)
        // and an expiry of 1 minute to 24 hours. Enforcing again only
        // tightens: Maintainer is enough.
        (
            Action::CodeownersEnforcementToggle,
            Resource::CodeownersEnforcement {
                project,
                highest_protection,
                change,
            },
        ) => {
            people_only()?;
            need(Role::Maintainer, project.role)?;
            let CodeownersToggle::Suspend { minutes } = change else {
                return Ok(());
            };
            match highest_protection {
                Some(level) if !admin && project.role.is_none_or(|r| *level > r) => {
                    return Err(deny(ErrorCode::Forbidden)
                        .with_context("reason", "above_own_role")
                        .with_context("required_role", level.as_str())
                        .with_context("actual_role", project.role.map_or("none", Role::as_str)));
                }
                Some(_) | None => {}
            }
            match minutes {
                None => Err(refused("expiry_missing")),
                Some(m) if *m == 0 || *m > MAX_CODEOWNERS_SUSPENSION_MINUTES => {
                    Err(refused("expiry_out_of_range")
                        .with_context("max_minutes", MAX_CODEOWNERS_SUSPENSION_MINUTES))
                }
                Some(_) => Ok(()),
            }
        }

        // --- discussions (M3 R3, ADR-0024 §12) ---
        // Commenting: Reporter and above by membership (an internal
        // project's non-member reads but does not write); an agent's token
        // and a service account may (a comment is no merge condition).
        (
            Action::MergeRequestComment,
            Resource::MergeRequest {
                project,
                source_branch,
                target_branch,
                ..
            },
        ) => {
            need(Role::Reporter, project.role)?;
            branches(source_branch, target_branch).map_err(refused)
        }
        // Linking a work item by hand (M3 I1b): Developer and above (plan
        // 11 §4.3), any state (a merged one keeps its links, 02 §5.7).
        (
            Action::WorkItemLink,
            Resource::MergeRequest {
                project,
                source_branch,
                target_branch,
                ..
            },
        ) => {
            need(Role::Developer, project.role)?;
            branches(source_branch, target_branch).map_err(refused)
        }
        // Resolving: people only; one's own thread (Reporter), another's
        // only a Developer who is not the merge request's author — the
        // author never clears another's thread, administrators included.
        (
            Action::DiscussionResolve,
            Resource::Discussion {
                project,
                mr_author,
                started_by,
            },
        ) => {
            people_only()?;
            if *started_by == user {
                return need(Role::Reporter, project.role);
            }
            if *mr_author == user {
                return Err(refused("author_foreign_discussion"));
            }
            need(Role::Developer, project.role)
        }

        // --- service accounts (M3 S0, ADR-0019 Addendum S0) ---
        // Creating one is a person's, an administrator's web session (a
        // token was refused above as session-only).
        (Action::ServiceAccountCreate, Resource::Instance) => {
            people_only()?;
            admin_only()
        }
        (Action::ServiceAccountList, Resource::Instance) => admin_only(),
        // Reading, listing and revoking its tokens: administrator rights
        // (an `admin`-scoped token included: a leak is answered at once).
        (
            Action::ServiceAccountRead
            | Action::ServiceAccountTokenList
            | Action::ServiceAccountTokenRevoke,
            Resource::ServiceAccount(account),
        ) => {
            admin_only()?;
            service_account(account)
        }
        // Who answers for it, and why it exists: a person's decision.
        (Action::ServiceAccountUpdate, Resource::ServiceAccount(account)) => {
            people_only()?;
            admin_only()?;
            service_account(account)
        }
        // Issuing its token: a person in a web session (refused above for
        // every token), with administrator rights, for an active service
        // account, never with `admin` (a service account has no
        // administrator rights to lend).
        (
            Action::ServiceAccountTokenIssue,
            Resource::ServiceAccountTokenRequest { account, scopes },
        ) => {
            people_only()?;
            admin_only()?;
            service_account(account)?;
            if !account.active {
                return Err(refused("target_disabled"));
            }
            if scopes.contains(&Scope::Admin) {
                return Err(refused("admin_scope_for_service_account"));
            }
            Ok(())
        }

        // --- check runs (M3 C1, ADR-0024 §10) ---
        // Visibility, pending deletion and the `api` scope were decided
        // above. Reporting is a service account's: an agent's token is
        // refused first (a person's agent must not turn its own merge
        // request green), then the namespace decides.
        (
            Action::CheckWrite,
            Resource::CheckRun {
                project,
                namespace,
                granted,
                reserved,
            },
        ) => {
            // M4 C5 (ADR-0029 decision 7): `ci/pipeline` is the system's,
            // written by forge's worker; refused to everyone over HTTP —
            // a granted service account and administrators included.
            if *reserved {
                return Err(
                    refused("ci_namespace_internal").with_context("namespace", namespace.as_str())
                );
            }
            if principal.is_agent() {
                return Err(refused("agent_token"));
            }
            match namespace {
                // forge's own worker writes these without HTTP (X2):
                // administrators are refused too.
                forge_core::CheckNamespace::Forge => Err(refused("forge_namespace_internal")),
                // A person's external tool (ADR-0024 §10): not in C1.
                forge_core::CheckNamespace::Ext => Err(refused("ext_namespace_unavailable")),
                forge_core::CheckNamespace::Ci | forge_core::CheckNamespace::Kanflow => {
                    if kind != AccountKind::ServiceAccount {
                        return Err(refused("not_service_account"));
                    }
                    if !granted {
                        return Err(refused("namespace_not_granted")
                            .with_context("namespace", namespace.as_str()));
                    }
                    // The membership role (a service account never holds
                    // administrator rights).
                    if project.role.is_some_and(|r| r >= Role::Developer) {
                        Ok(())
                    } else {
                        Err(deny(ErrorCode::Forbidden)
                            .with_context("required_role", Role::Developer.as_str())
                            .with_context("actual_role", project.role.map_or("none", Role::as_str)))
                    }
                }
            }
        }
        (Action::CheckRead, Resource::Project(p)) => need(Role::Reporter, read_role(p)),
        (Action::CheckNamespaceGrantManage, Resource::ServiceAccount(account)) => {
            people_only()?;
            admin_only()?;
            service_account(account)
        }
        // --- integrations (M3 I1a, plan 11 §5 I1) ---
        // Creating and changing one: a person's, an administrator's web
        // session (a token was refused above as session-only).
        (Action::IntegrationManage, Resource::Instance) => {
            people_only()?;
            admin_only()
        }
        // Reading them: administrators, people only (an `admin`-scoped
        // token of a person included).
        (Action::IntegrationRead, Resource::Instance) => {
            people_only()?;
            admin_only()
        }
        // Binding a project, changing or removing a binding (M3 X3): people
        // only, Maintainer (visibility and pending deletion were decided
        // above: FG-REPO-0005 / FG-REPO-0006). The integration's service
        // account never binds on its own (people only); acting for a
        // person it needs the person's assertion (act-as arm).
        (
            Action::IntegrationBindingManage | Action::IntegrationBindingRemove,
            Resource::Project(p),
        ) => {
            people_only()?;
            need(Role::Maintainer, p.role)
        }
        // The repositories bound to an integration (M3 X3): its own
        // service account only, as its feed.
        (Action::IntegrationBindingList, Resource::IntegrationFeed(feed)) => {
            integration_feed(principal, feed).map_err(|e| e.with_context("action", action.name()))
        }
        // The repository search (M3 X3): the route for any active caller
        // (the scope was checked above); each candidate only when the
        // caller could bind it (Maintainer; visibility was decided above).
        // Review LOW (b): a person's (or acted for, decided as the person);
        // never a service account, a bot or an agent on its own.
        (Action::RepositorySearch, Resource::Instance) => people_only(),
        (Action::RepositorySearch, Resource::Project(p)) => need(Role::Maintainer, p.role),

        // --- the integrations' event feed (M3 I1c, ADR-0016 §7) ---
        // The route: the integration's own service account only.
        (
            Action::IntegrationEventRead
            | Action::IntegrationEventCursorRead
            | Action::IntegrationDevStatusRead,
            Resource::IntegrationFeed(feed),
        ) => integration_feed(principal, feed).map_err(|e| e.with_context("action", action.name())),
        // One bound project's events of one class (`prove_visible`):
        // visibility and pending deletion were decided above.
        (
            Action::IntegrationEventRead
            | Action::IntegrationEventCursorRead
            | Action::IntegrationDevStatusRead,
            Resource::ProjectEvents { project, class },
        ) => project_events(principal, project, *class)
            .map_err(|e| e.with_context("action", action.name())),

        // --- acting for a person (M3 S2, ADR-0025 §5) ---
        // The confirmation: a person's web session (a token was refused
        // above as session-only) that sees the merge request, for an
        // enabled integration bound to its project, and whose session may
        // perform the intent's action on it now.
        // M3 X3: a binding intent's confirmation is on the project (read
        // like the project); `binding_set` needs no binding yet.
        (Action::ActAsConfirmView | Action::ActAsAssertionIssue, Resource::ActAsConfirm(c)) => {
            use forge_core::act_as::Intent;
            people_only()?;
            let read = if c.intent.is_merge_request() {
                Action::MergeRequestRead
            } else {
                Action::ProjectRead
            };
            authorize(principal, read, &c.target)
                .map_err(|e| e.with_context("action", action.name()))?;
            // Review LOW (a): the person's own right first, so the
            // integration's state is said only to who may act.
            let intended = match c.intent {
                Intent::Approve => Action::MergeRequestApprove,
                Intent::Unapprove => Action::MergeRequestUnapprove,
                Intent::BindingSet => Action::IntegrationBindingManage,
                Intent::BindingRemove => Action::IntegrationBindingRemove,
            };
            authorize(principal, intended, &c.target).map_err(|e| {
                e.with_context("action", action.name())
                    .with_context("intent", c.intent.as_str())
            })?;
            if !c.integration_enabled {
                return Err(refused("integration_disabled"));
            }
            if !c.bound && c.intent != Intent::BindingSet {
                return Err(refused("act_as_unbound"));
            }
            Ok(())
        }
        // The claim: the integration's own service account (as its feed).
        (Action::ActAsAssertionClaim, Resource::IntegrationFeed(feed)) => {
            integration_feed(principal, feed).map_err(|e| e.with_context("action", action.name()))
        }
        // Only the job arm (returned above) reads a pinned copy (ADR-0029
        // §3): a person, a token or a service account never gets one.
        (_, Resource::PinnedCopy(_)) => {
            Err(deny(ErrorCode::Forbidden).with_context("reason", "pinned_copy_without_job"))
        }
        // --- CI runners (M4 C3, ADR-0029 §1) ---
        // The runner API is a runner's alone (the job arm, returned above):
        // a person, a token or a service account never reaches it.
        (Action::RunnerJobRequest, Resource::Instance)
        | (
            Action::RunnerJobHeartbeat | Action::RunnerLogAppend | Action::RunnerJobComplete,
            Resource::RunnerJob(_),
        ) => Err(refused("runner_only")),
        // Registering and managing runners: a person, an administrator's
        // web session (a token was refused above as session-only).
        (Action::RunnerRegister | Action::RunnerManage, Resource::Instance) => {
            people_only()?;
            admin_only()
        }
        // Only the act-as arm (returned above) reads an act-as resource.
        (_, Resource::ActAs(_)) => {
            Err(deny(ErrorCode::Forbidden).with_context("reason", "act_as_resource_without_act_as"))
        }
        // --- webhooks (M3 I2, ADR-0016 §8, plan 11 §4.3) ---
        // A project's Maintainers (visibility and pending deletion were
        // decided above: FG-REPO-0005 / FG-REPO-0006). Managing and
        // redelivering are people's.
        (Action::WebhookRead | Action::WebhookDeliveryRead, Resource::Project(p)) => {
            need(Role::Maintainer, p.role)
        }
        (Action::WebhookManage | Action::WebhookRedeliver, Resource::Project(p)) => {
            people_only()?;
            need(Role::Maintainer, p.role)
        }
        // A delivery, for the webhook's owner (`prove_webhook_delivery`):
        // still a Maintainer of the project; every class but admin (the
        // maintainer and deployment classes need Maintainer and Developer,
        // which the owner holds; the webhook names them explicitly).
        (Action::WebhookDeliver, Resource::ProjectEvents { project, class }) => {
            people_only()?;
            need(Role::Maintainer, project.role)?;
            match class {
                forge_core::VisibilityClass::Integration
                | forge_core::VisibilityClass::Maintainer
                | forge_core::VisibilityClass::Deployment => Ok(()),
                forge_core::VisibilityClass::Admin => {
                    Err(refused("class_not_for_webhooks").with_context("class", class.as_str()))
                }
            }
        }

        // --- the push mirror (M3 O2, ADR-0024 Amendment O2) ---
        // Administrators, people only (visibility and pending deletion were
        // decided above: an administrator sees every project; a token
        // without `admin` holds no administrator rights).
        (Action::PushMirrorRead | Action::PushMirrorManage, Resource::Project(_)) => {
            people_only()?;
            admin_only()
        }

        // --- documents (plan 13 Q13, ADR-0026) ---
        // The migration downgrade of broken references: a governance
        // setting like the merge settings — people only, Maintainer.
        (Action::DocLintMigrationManage, Resource::Project(p)) => {
            people_only()?;
            need(Role::Maintainer, p.role)
        }

        // --- CI pipelines (M4 C2, plan 14 §4.2) ---
        // Reading: as RepoBrowse (an agent audience too: metadata only,
        // CI outputs are decided on their marks from C5). Running by hand,
        // cancelling and retrying: Developer and above by membership
        // (agents and service accounts included, plan 14 §4.2).
        (Action::PipelineRead, Resource::Project(p)) => need(Role::Reporter, read_role(p)),
        // M4 C5 (ADR-0029 decision 5): a CI output is read like the
        // project (the route's proof)...
        (Action::CiLogRead | Action::PipelineDiagnosisRead, Resource::Project(p)) => {
            need(Role::Reporter, read_role(p))
        }
        // ...then, with the marks (`authorize_ci_output`), an AI agent
        // audience only when both the pipeline's mark and the default
        // branch's tip are `Open`. The pipeline's mark first: it is what
        // the job saw. `Undecided`, `Unborn` and `NotMeasured` are closed.
        (
            Action::CiLogRead | Action::PipelineDiagnosisRead,
            Resource::CiOutput {
                project,
                pipeline_mark,
                tip_mark,
                declared_agent,
            },
        ) => {
            need(Role::Reporter, read_role(project))?;
            let audience = if principal.is_agent_audience() {
                audience_of(principal)
            } else if *declared_agent {
                "header"
            } else {
                return Ok(());
            };
            let route = if action == Action::CiLogRead {
                "ci_log"
            } else {
                "ci_diagnosis"
            };
            let (reason, mark_of) = if *pipeline_mark != CopyMark::Open {
                (pipeline_mark.pipeline_reason(), "pipeline")
            } else if *tip_mark != CopyMark::Open {
                (tip_mark.as_str(), "tip")
            } else {
                return Ok(());
            };
            Err(deny(ErrorCode::RepositoryCopyWithheld)
                .with_context("route", route)
                .with_context("reason", reason)
                .with_context("mark_of", mark_of)
                .with_context("audience", audience))
        }
        (
            Action::PipelineCreate | Action::PipelineCancel | Action::PipelineRetry,
            Resource::Project(p),
        ) => need(Role::Developer, p.role),

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
            | Action::SpecRefRead
            | Action::SpecRead
            | Action::ContextRead
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
            | Action::NotificationPreferencesRead
            | Action::NotificationInboxRead
            | Action::UserMergeRequestList
            | Action::ActivityRead
            | Action::NotificationPreferencesWrite
            | Action::NotificationInboxUpdate
            | Action::UserCreate
            | Action::UserDisable
            | Action::UserSearch
            | Action::UserAdminList
            | Action::AuditRead
            | Action::UserSelfRead
            | Action::SessionRead
            | Action::SessionList
            | Action::SessionRevoke
            | Action::AdminSessionList
            | Action::AdminSessionRevoke
            | Action::SessionCreate
            | Action::SessionContinue
            | Action::SessionLogout
            | Action::PathResolve
            | Action::MergeRequestCreate
            | Action::MergeRequestRead
            | Action::MergeRequestList
            | Action::MergeRequestUpdate
            | Action::MergeRequestClose
            | Action::MergeRequestReopen
            | Action::MergeRequestVersionRefsPurge
            | Action::MergeRequestComment
            | Action::WorkItemLink
            | Action::DiscussionResolve
            | Action::MergeRequestMerge
            | Action::MergeRequestApprove
            | Action::MergeRequestUnapprove
            | Action::ApprovalRuleManage
            | Action::MergeSettingsManage
            | Action::CodeownersEnforcementToggle
            | Action::ServiceAccountCreate
            | Action::ServiceAccountList
            | Action::ServiceAccountRead
            | Action::ServiceAccountUpdate
            | Action::ServiceAccountTokenIssue
            | Action::ServiceAccountTokenList
            | Action::ServiceAccountTokenRevoke
            | Action::CheckWrite
            | Action::CheckRead
            | Action::CheckNamespaceGrantManage
            | Action::IntegrationManage
            | Action::IntegrationRead
            | Action::IntegrationBindingManage
            | Action::IntegrationBindingRemove
            | Action::IntegrationBindingList
            | Action::RepositorySearch
            | Action::IntegrationEventRead
            | Action::IntegrationEventCursorRead
            | Action::IntegrationDevStatusRead
            | Action::ActAsConfirmView
            | Action::ActAsAssertionIssue
            | Action::ActAsAssertionClaim
            | Action::WebhookRead
            | Action::WebhookManage
            | Action::WebhookDeliveryRead
            | Action::WebhookRedeliver
            | Action::WebhookDeliver
            | Action::PushMirrorRead
            | Action::PushMirrorManage
            | Action::DocLintMigrationManage
            | Action::PipelineRead
            | Action::CiLogRead
            | Action::PipelineDiagnosisRead
            | Action::PipelineCreate
            | Action::PipelineCancel
            | Action::PipelineRetry
            | Action::RunnerRegister
            | Action::RunnerManage
            | Action::RunnerJobRequest
            | Action::RunnerJobHeartbeat
            | Action::RunnerLogAppend
            | Action::RunnerJobComplete
            | Action::Metrics,
            Resource::Instance
            | Resource::Group { .. }
            | Resource::Namespace { .. }
            | Resource::Project(_)
            | Resource::RepoCopy { .. }
            | Resource::Ref { .. }
            | Resource::Transfer { .. }
            | Resource::GroupTransfer { .. }
            | Resource::UserRename { .. }
            | Resource::Tombstone(_)
            | Resource::Membership { .. }
            | Resource::RefProtectionChange { .. }
            | Resource::VisibilityChange { .. }
            | Resource::Token { .. }
            | Resource::NotificationPreferences { .. }
            | Resource::UserMergeRequests { .. }
            | Resource::UserActivity { .. }
            | Resource::Session { .. }
            | Resource::TokenRequest { .. }
            | Resource::Admission { .. }
            | Resource::Resolved { .. }
            | Resource::MergeRequest { .. }
            | Resource::Discussion { .. }
            | Resource::CodeownersEnforcement { .. }
            | Resource::MergeSettingsChange { .. }
            | Resource::ServiceAccount(_)
            | Resource::ServiceAccountTokenRequest { .. }
            | Resource::CheckRun { .. }
            | Resource::IntegrationFeed(_)
            | Resource::ProjectEvents { .. }
            | Resource::ActAsConfirm(_)
            | Resource::CiOutput { .. }
            | Resource::RunnerJob(_),
        ) => wrong_resource(),
    }
}

/// Why `principal` counts as an AI agent audience (the log `audience`,
/// as the context API names it).
fn audience_of(principal: &Principal) -> &'static str {
    if principal.is_agent() {
        return "agent_token";
    }
    match principal.account_kind() {
        AccountKind::ServiceAccount => "service_account",
        AccountKind::Bot => "bot",
        AccountKind::Human => "person",
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
        Resource::RepoCopy { project, .. } => live(project),
        Resource::Ref { project, .. } => live(project),
        Resource::Transfer { source, .. } => live(source),
        Resource::RefProtectionChange { project, .. }
        | Resource::VisibilityChange { project, .. }
        | Resource::MergeRequest { project, .. }
        | Resource::Discussion { project, .. }
        | Resource::CodeownersEnforcement { project, .. }
        | Resource::MergeSettingsChange { project, .. }
        | Resource::CheckRun { project, .. }
        | Resource::CiOutput { project, .. }
        | Resource::ProjectEvents { project, .. } => live(project),
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
        | Resource::NotificationPreferences { .. }
        | Resource::UserMergeRequests { .. } | Resource::UserActivity { .. }
        | Resource::Session { .. }
        | Resource::TokenRequest { .. }
        | Resource::Admission { .. }
        | Resource::GroupTransfer { .. }
        | Resource::UserRename { .. }
        | Resource::Tombstone(_)
        | Resource::ServiceAccount(_)
        | Resource::ServiceAccountTokenRequest { .. }
        | Resource::IntegrationFeed(_)
        // Decided in their own arms (the act-as arm; the confirmation
        // decides visibility on its merge request; the job arm).
        | Resource::PinnedCopy(_)
        | Resource::RunnerJob(_)
        | Resource::ActAs(_)
        | Resource::ActAsConfirm(_) => None,
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
        | Resource::RepoCopy { .. }
        | Resource::PinnedCopy(_)
        | Resource::RunnerJob(_)
        | Resource::Transfer { .. }
        | Resource::GroupTransfer { .. }
        | Resource::UserRename { .. }
        | Resource::Membership { .. }
        | Resource::RefProtectionChange { .. }
        | Resource::VisibilityChange { .. }
        | Resource::Token { .. }
        | Resource::NotificationPreferences { .. }
        | Resource::UserMergeRequests { .. }
        | Resource::UserActivity { .. }
        | Resource::Session { .. }
        | Resource::TokenRequest { .. }
        | Resource::Admission { .. }
        | Resource::Unresolved(_)
        | Resource::Resolved { .. }
        | Resource::MergeRequest { .. }
        | Resource::Discussion { .. }
        | Resource::CodeownersEnforcement { .. }
        | Resource::MergeSettingsChange { .. }
        | Resource::ServiceAccount(_)
        | Resource::ServiceAccountTokenRequest { .. }
        | Resource::CheckRun { .. }
        | Resource::CiOutput { .. }
        | Resource::IntegrationFeed(_)
        | Resource::ProjectEvents { .. }
        | Resource::ActAs(_)
        | Resource::ActAsConfirm(_) => None,
    }
}

/// A merge request's branches (ADR-0024 §1): both `refs/heads/*`, never
/// the same. The denial's log `reason` otherwise.
fn branches(
    source: &forge_core::RefName,
    target: &forge_core::RefName,
) -> Result<(), &'static str> {
    if RefKind::of(source) != RefKind::Branch || RefKind::of(target) != RefKind::Branch {
        Err("not_a_branch")
    } else if source == target {
        Err("source_equals_target")
    } else {
        Ok(())
    }
}

/// Members see their projects; every signed-in person sees internal ones,
/// a service account only its own (M3 S0, plan 11 §5 S0: an integration
/// reads what it was made a member of, nothing else). (Administrator rights
/// are checked by the caller.)
fn can_see(kind: AccountKind, p: &ProjectView) -> bool {
    p.role.is_some() || (p.visibility == Visibility::Internal && sees_internal(kind))
}

/// Whether the account sees internal projects without a membership.
fn sees_internal(kind: AccountKind) -> bool {
    match kind {
        AccountKind::Human | AccountKind::Bot => true,
        AccountKind::ServiceAccount => false,
    }
}

/// The role that applies to reading: on an internal project every signed-in
/// person reads like a Reporter (a service account by its membership only).
fn read_role(kind: AccountKind, p: &ProjectView) -> Option<Role> {
    match p.visibility {
        Visibility::Internal if sees_internal(kind) => {
            Some(p.role.map_or(Role::Reporter, |r| r.max(Role::Reporter)))
        }
        Visibility::Internal | Visibility::Private => p.role,
    }
}

/// A service account route's target must be one: another kind is the same
/// 404 as an absent user (`reason=not_service_account`).
fn service_account(account: &ServiceAccountView) -> Result<(), ForgeError> {
    match account.kind {
        AccountKind::ServiceAccount => Ok(()),
        AccountKind::Human | AccountKind::Bot => {
            Err(ForgeError::new(ErrorCode::NotFound).with_context("reason", "not_service_account"))
        }
    }
}

/// The feed route (M3 I1c): the caller must be the integration's own
/// service account — a person (administrators included) or a bot is the
/// same 404 as another integration (no oracle for which ids exist), an
/// agent's token, an act-as request and a disabled integration are 403.
/// No administrator right passes anything here.
fn integration_feed(
    principal: &Principal,
    feed: &crate::model::IntegrationFeedView,
) -> Result<(), ForgeError> {
    let not_found =
        |reason| Err(ForgeError::new(ErrorCode::NotFound).with_context("reason", reason));
    let refused =
        |reason| Err(ForgeError::new(ErrorCode::Forbidden).with_context("reason", reason));
    match principal.account_kind() {
        AccountKind::ServiceAccount => {}
        AccountKind::Human | AccountKind::Bot => return not_found("not_integration_account"),
    }
    if !feed.own {
        return not_found("foreign_integration");
    }
    if principal.is_agent() {
        return refused("agent_token");
    }
    if feed.act_as {
        return refused("act_as_refused");
    }
    if !feed.enabled {
        return refused("integration_disabled");
    }
    Ok(())
}

/// One bound project's events of one class for the feed's principal (M3
/// I1c): the integration class by a Reporter (or higher) membership of the
/// service account; the maintainer, deployment and admin classes never
/// (they are not the feed's, whatever the role). No administrator right
/// passes.
fn project_events(
    principal: &Principal,
    project: &ProjectView,
    class: forge_core::VisibilityClass,
) -> Result<(), ForgeError> {
    let refused =
        |reason| Err(ForgeError::new(ErrorCode::Forbidden).with_context("reason", reason));
    match principal.account_kind() {
        AccountKind::ServiceAccount => {}
        AccountKind::Human | AccountKind::Bot => return refused("not_service_account"),
    }
    if principal.is_agent() {
        return refused("agent_token");
    }
    match class {
        forge_core::VisibilityClass::Integration => {}
        forge_core::VisibilityClass::Maintainer
        | forge_core::VisibilityClass::Deployment
        | forge_core::VisibilityClass::Admin => {
            return Err(ForgeError::new(ErrorCode::Forbidden)
                .with_context("reason", "class_not_in_feed")
                .with_context("class", class.as_str()))
        }
    }
    // The membership role: a service account sees member projects only,
    // and holds no administrator right to lend.
    if project.role.is_some_and(|r| r >= Role::Reporter) {
        Ok(())
    } else {
        Err(ForgeError::new(ErrorCode::Forbidden)
            .with_context("required_role", Role::Reporter.as_str())
            .with_context("actual_role", project.role.map_or("none", Role::as_str)))
    }
}
