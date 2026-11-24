//! The permission matrix rows of the PFS portal's widgets (ADR-0036):
//!
//! - The portal's credential (`W::Portal`) reads the widgets
//!   (`PortalWidgetRead`) on the instance when its party is the portal, it
//!   holds the access role and the person is active and a person; each
//!   condition has its deny row.
//! - A project a widget shows is read like the person would in a session
//!   without administrator rights: a private project without a role is the
//!   invisible project's 404, one pending deletion `FG-REPO-0006`.
//! - Nobody else reads the widgets: a session, a token, an administrator,
//!   the CI and bot principals are refused (`portal_credential_required`
//!   or their own arm). An unlinked staff id never becomes a principal (the
//!   anonymous row; forge-web answers `FG-AUTH-0027` before `authorize()`).
//! - The portal's credential is refused every other action
//!   ([`generated_denies`]).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

/// The portal credentials of the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Case {
    /// Party portal, access role, active person, no role on the project.
    Ok,
    /// The same, a Reporter on the project.
    Reporter,
    /// The token holds no GitStation access role.
    NoRole,
    /// The token was issued to another client (`azp`).
    WrongParty,
    /// The person's account is disabled.
    Disabled,
    /// The linked account is a service account.
    ServiceAccount,
}

#[cfg(test)]
impl Case {
    pub(crate) fn principal(self) -> crate::model::Principal {
        use crate::model::{AccountKind, PortalCredential, Principal};
        let credential = PortalCredential {
            portal_party: self != Case::WrongParty,
            access_role: self != Case::NoRole,
        };
        let kind = if self == Case::ServiceAccount {
            AccountKind::ServiceAccount
        } else {
            AccountKind::Human
        };
        Principal::portal(
            crate::matrix::ME,
            self != Case::Disabled,
            kind,
            credential,
            false,
        )
    }

    pub(crate) fn role(self) -> Option<crate::model::Role> {
        (self == Case::Reporter).then_some(crate::model::Role::Reporter)
    }
}

const fn row(action: A, actor: W, fixture: F, expect: Expect) -> Row {
    Row {
        action,
        actor,
        fixture,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

const OK: W = W::Portal(Case::Ok);

#[rustfmt::skip]
pub(crate) const PORTAL_ROWS: &[Row] = &[
    // The widget route.
    row(A::PortalWidgetRead, OK, F::Instance, Allow),
    row(A::PortalWidgetRead, W::Portal(Case::Reporter), F::Instance, Allow),
    row(A::PortalWidgetRead, W::Portal(Case::NoRole), F::Instance, deny(E::Forbidden)),
    row(A::PortalWidgetRead, W::Portal(Case::WrongParty), F::Instance, deny(E::Forbidden)),
    row(A::PortalWidgetRead, W::Portal(Case::Disabled), F::Instance, deny(E::AccountDisabled)),
    row(A::PortalWidgetRead, W::Portal(Case::ServiceAccount), F::Instance, deny(E::Forbidden)),
    // A project a widget shows: as the person reads it.
    row(A::PortalWidgetRead, W::Portal(Case::Reporter), F::PrivateProject, Allow),
    row(A::PortalWidgetRead, OK, F::InternalProject, Allow),
    row(A::PortalWidgetRead, OK, F::PrivateProject, deny(E::NotFound)),
    row(A::PortalWidgetRead, W::Portal(Case::Reporter), F::DeletedProject, deny(E::PendingDeletion)),
    row(A::PortalWidgetRead, W::Portal(Case::NoRole), F::InternalProject, deny(E::Forbidden)),
    row(A::PortalWidgetRead, W::Portal(Case::WrongParty), F::InternalProject, deny(E::Forbidden)),
    // No other resource.
    row(A::PortalWidgetRead, OK, F::OwnUserMergeRequests, deny(E::Forbidden)),
    // Widget design P1: `inbox`, `attention-count` and the new `my-reviews`
    // kinds read on the widget route (the instance) only — never the
    // person's notifications or another person's lists as a resource.
    row(A::PortalWidgetRead, OK, F::OwnNotificationPreferences, deny(E::Forbidden)),
    row(A::PortalWidgetRead, W::Portal(Case::Reporter), F::ForeignUserMergeRequests, deny(E::Forbidden)),
    row(A::NotificationInboxRead, OK, F::OwnNotificationPreferences, deny(E::Forbidden)),
    row(A::UserMergeRequestList, OK, F::OwnUserMergeRequests, deny(E::Forbidden)),
    // Nobody but the portal's credential.
    row(A::PortalWidgetRead, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    row(A::PortalWidgetRead, W::Disabled, F::Instance, deny(E::AccountDisabled)),
    row(A::PortalWidgetRead, W::Developer, F::Instance, deny(E::Forbidden)),
    row(A::PortalWidgetRead, W::Admin, F::Instance, deny(E::Forbidden)),
    row(A::PortalWidgetRead, W::DevTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::PortalWidgetRead, W::OwnerTokenReadApi, F::Instance, deny(E::Forbidden)),
    row(A::PortalWidgetRead, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::PortalWidgetRead, W::Runner, F::Instance, deny(E::Forbidden)),
    row(A::PortalWidgetRead, W::AutoMerge, F::Instance, deny(E::Forbidden)),
    row(A::PortalWidgetRead, W::Autofix, F::Instance, deny(E::Forbidden)),
];

/// The portal's credential is refused every action but `PortalWidgetRead`,
/// on the instance and on a project it could read (`portal_not_allowed`;
/// the act-as confirmations answer `FG-REPO-0005` as for everyone they
/// refuse).
pub(crate) fn generated_denies() -> Vec<Row> {
    A::ALL
        .iter()
        .filter(|a| !crate::portal::portal_may(**a))
        .flat_map(|&a| {
            let code = if matches!(a, A::ActAsConfirmView | A::ActAsAssertionIssue) {
                E::NotFound
            } else {
                E::Forbidden
            };
            [
                row(a, W::Portal(Case::Reporter), F::Instance, deny(code)),
                row(a, W::Portal(Case::Reporter), F::PrivateProject, deny(code)),
            ]
        })
        .collect()
}
