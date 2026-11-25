//! The PFS portal's arm of `authorize()` (ADR-0036). `authorize()` calls
//! [`decide`] first for every `Credential::Portal` (as it does the CI,
//! auto-merge and autofix arms), so no other rule — the public actions,
//! the roles, the scopes — is ever reached by the portal's credential.
//!
//! Closed by default ([`portal_may`]): `PortalWidgetRead` only, and only
//!
//! - when the token's party is the portal (`portal_wrong_party`),
//! - when it holds the GitStation access role (`portal_no_access_role`),
//! - for an active person (`FG-AUTH-0004`; a service account or a bot is
//!   `not_a_person`: the portal speaks for people),
//! - on `Resource::Instance` (the widget route: the person's own lists) or
//!   on `Resource::Project` (each project a widget shows or offers). A
//!   project is decided as `ProjectRead` **for the person in a session
//!   without administrator rights** ([`reader`]) — the same rule the web UI
//!   shows the project by, so a private project the person holds no role
//!   on is the invisible project's `FG-REPO-0005`, a project pending
//!   deletion `FG-REPO-0006`, and an administrator sees through the portal
//!   only what their own roles show.
//!
//! Every other action, and every other resource, is `FG-AUTH-0003
//! reason=portal_not_allowed` / `portal_resource`. A merge request a list
//! widget shows is decided with [`portal_reader`]'s `MergeRequestRead` (and
//! the approval trial of the "review" list), on the route's proof.

use forge_core::UserId;
use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::model::{AccountKind, Credential, PortalCredential, Principal, Resource};
use crate::prove::{sealed, Authorized, ResourceCtx};

/// Whether the portal's credential may perform `action` at all.
pub fn portal_may(action: Action) -> bool {
    matches!(action, Action::PortalWidgetRead)
}

/// Decide `action` on `resource` for the portal's credential.
pub(crate) fn decide(
    principal: &Principal,
    credential: PortalCredential,
    action: Action,
    resource: &Resource,
) -> Result<(), ForgeError> {
    let deny = |code: ErrorCode, reason: &'static str| {
        ForgeError::new(code)
            .with_context("action", action.name())
            .with_context("credential", "portal")
            .with_context("reason", reason)
    };
    if !portal_may(action) {
        return Err(deny(ErrorCode::Forbidden, "portal_not_allowed"));
    }
    if principal.user().is_none() {
        return Err(deny(ErrorCode::Unauthenticated, "portal_without_user"));
    }
    if !credential.portal_party {
        return Err(deny(ErrorCode::Forbidden, "portal_wrong_party"));
    }
    if !credential.access_role {
        return Err(deny(ErrorCode::Forbidden, "portal_no_access_role"));
    }
    if !principal.active() {
        return Err(deny(ErrorCode::AccountDisabled, "owner_disabled"));
    }
    if principal.account_kind() != AccountKind::Human {
        return Err(deny(ErrorCode::Forbidden, "not_a_person"));
    }
    if matches!(resource, Resource::Instance) {
        return Ok(());
    }
    // A project is the only other thing a widget reads; anything else is
    // refused whatever the person may do elsewhere.
    if !matches!(resource, Resource::Project(_)) {
        return Err(deny(ErrorCode::Forbidden, "portal_resource"));
    }
    let person =
        reader(principal).ok_or_else(|| deny(ErrorCode::Unauthenticated, "portal_without_user"))?;
    crate::authorize::authorize(&person, Action::ProjectRead, resource)
        .map_err(|e| e.with_context("via", "portal_widget"))
}

/// The person behind a portal principal, as the web UI decides for them:
/// a session without administrator rights. `None` for any other principal.
fn reader(principal: &Principal) -> Option<Principal> {
    principal.portal_credential()?;
    Some(Principal::authenticated(
        principal.user()?,
        false,
        principal.active(),
        Credential::Session,
        principal.account_kind(),
    ))
}

/// The person a widget route was allowed for (`PortalWidgetRead` on the
/// instance), to decide each listed merge request with the existing rules
/// (`MergeRequestRead`, the approval trial): a session without
/// administrator rights. Only a proof of the widget route yields it.
pub fn portal_reader(proof: &Authorized<PortalWidgetCtx>) -> Result<Principal, ForgeError> {
    if proof.action() != Action::PortalWidgetRead {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_action_mismatch")
            .with_context("action", proof.action().name()));
    }
    reader(proof.principal()).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "portal_reader_without_portal")
    })
}

/// Whether the menu widget shows the person the administrator link
/// (ADR-0021 amendment 2026-11-24): `authorize()` decides
/// `UserAdminList` on the instance — the administrator screen's own
/// decision — for the person as a session with the account's
/// administrator flag (`users.is_admin` as read with the request; kept to
/// `auth.oidc.admin_role` at the GitStation sign-ins, never read from the
/// portal's token). Only a proof of the widget route answers `true`; the
/// link grants nothing: the screen asks for a GitStation session.
pub fn portal_admin_menu(proof: &Authorized<PortalWidgetCtx>) -> bool {
    let principal = proof.principal();
    if proof.action() != Action::PortalWidgetRead || principal.portal_credential().is_none() {
        return false;
    }
    let Some(user) = principal.user() else {
        return false;
    };
    let person = Principal::authenticated(
        user,
        principal.admin_flag(),
        principal.active(),
        Credential::Session,
        principal.account_kind(),
    );
    crate::authorize::authorize(&person, Action::UserAdminList, &Resource::Instance).is_ok()
}

/// A widget route as the portal reaches it: the person's own widgets. The
/// id is the person (the lists are always their own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalWidgetCtx {
    user: UserId,
}

impl PortalWidgetCtx {
    pub fn new(user: UserId) -> Self {
        Self { user }
    }
}

impl sealed::Sealed for PortalWidgetCtx {}

impl ResourceCtx for PortalWidgetCtx {
    type Id = UserId;
    fn resource(&self) -> Resource {
        Resource::Instance
    }
    fn id(&self) -> UserId {
        self.user
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Visibility;
    use crate::prove::{prove, LoadedProject, ProjectCtx};

    const ME: UserId = UserId::new(1);

    fn portal(party: bool, role: bool) -> Principal {
        portal_of(party, role, false)
    }

    fn portal_of(party: bool, role: bool, is_admin: bool) -> Principal {
        Principal::portal(
            ME,
            true,
            AccountKind::Human,
            PortalCredential {
                portal_party: party,
                access_role: role,
            },
            is_admin,
        )
    }

    /// ADR-0021 amendment 2026-11-24: the administrator link follows the
    /// account's flag through `authorize()`; the reader of the lists still
    /// has no administrator rights.
    #[test]
    fn the_admin_menu_follows_the_account_flag_and_grants_nothing() {
        let admin = portal_of(true, true, true);
        let proof =
            prove(&admin, Action::PortalWidgetRead, PortalWidgetCtx::new(ME)).expect("allowed");
        assert!(portal_admin_menu(&proof));
        assert!(
            !admin.admin_rights(),
            "the portal's credential has no rights"
        );
        assert!(!portal_reader(&proof).expect("reader").admin_rights());
        let member = portal_of(true, true, false);
        let proof =
            prove(&member, Action::PortalWidgetRead, PortalWidgetCtx::new(ME)).expect("allowed");
        assert!(!portal_admin_menu(&proof));
    }

    #[test]
    fn the_reader_is_the_person_without_administrator_rights() {
        let proof = prove(
            &portal(true, true),
            Action::PortalWidgetRead,
            PortalWidgetCtx::new(ME),
        )
        .expect("allowed");
        let person = portal_reader(&proof).expect("reader");
        assert_eq!(person.user(), Some(ME));
        assert!(!person.admin_rights());
        assert_eq!(person.credential(), &Credential::Session);
    }

    /// Widget design P2b: the `deploy-approvals` trial is `DeployApprove`
    /// for the reader — a session without administrator rights — so an
    /// administrator's account never sees a deployment through the
    /// administrator bypass, and the portal's credential itself never
    /// approves.
    #[test]
    fn the_deploy_approval_trial_has_no_administrator_rights() {
        use crate::model::{ApproverGate, DeploymentView, ProjectView};
        let deployment = |role| {
            Resource::Deployment(DeploymentView {
                project: ProjectView {
                    visibility: Visibility::Private,
                    role,
                    pending_deletion: false,
                },
                tier: forge_core::deploy::DeployTier::Prod,
                requested_by: Some(UserId::new(9)),
                change_authors: Vec::new(),
                approvers: ApproverGate::Unrestricted,
                on_behalf_of: None,
                delegator_role: None,
                delegation_refused: None,
            })
        };
        let admin = portal_of(true, true, true);
        let proof =
            prove(&admin, Action::PortalWidgetRead, PortalWidgetCtx::new(ME)).expect("allowed");
        let reader = portal_reader(&proof).expect("reader");
        assert!(
            crate::authorize::authorize(&reader, Action::DeployApprove, &deployment(None)).is_err(),
            "no administrator bypass through the portal"
        );
        assert!(crate::authorize::authorize(
            &reader,
            Action::DeployApprove,
            &deployment(Some(crate::Role::Maintainer))
        )
        .is_ok());
        let Err(own) = crate::authorize::authorize(
            &admin,
            Action::DeployApprove,
            &deployment(Some(crate::Role::Maintainer)),
        ) else {
            panic!("the portal's credential must never approve");
        };
        assert_eq!(own.code(), ErrorCode::Forbidden);
    }

    #[test]
    fn a_project_is_read_like_the_person_would() {
        let private = |role| {
            ProjectCtx::new(LoadedProject {
                id: forge_core::ProjectId::new(5),
                visibility: Visibility::Private,
                role,
                pending_deletion: false,
            })
        };
        let p = portal(true, true);
        assert!(prove(
            &p,
            Action::PortalWidgetRead,
            private(Some(crate::Role::Reporter))
        )
        .is_ok());
        let Err(hidden) = prove(&p, Action::PortalWidgetRead, private(None)) else {
            panic!("a private project the person cannot see must stay hidden");
        };
        assert_eq!(hidden.code(), ErrorCode::NotFound);
    }
}
