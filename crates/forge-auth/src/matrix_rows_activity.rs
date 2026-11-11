//! The permission matrix rows of the activity list (plan 15 MR 8,
//! `GET /api/v1/user/activity`):
//!
//! - the route (`ActivityRead` on `Fixture::OwnUserActivity` /
//!   `ForeignUserActivity`): only the person's own, people only (an
//!   agent's token, a service account and a bot are refused), `read_api`;
//!   another user's is not found, administrators included. Act-as and CI
//!   credentials: the generated rows (`ActAsRule::Denied`, `CiRule`).
//! - one listed event (`ActivityRead` on `Fixture::ProjectEvents`, decided
//!   by forge-review per project and class): Reporter and above by role,
//!   the integration class only (never the maintainer, deployment or admin
//!   class, administrators and Owners included); an internal project the
//!   person only reads is refused (no role), a private one without a role
//!   is not found, a pending deletion is refused.

use forge_core::VisibilityClass as C;
use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};
use crate::matrix_review::MrProject as P;

const fn row(actor: W, fixture: F, expect: Expect) -> Row {
    Row {
        action: A::ActivityRead,
        actor,
        fixture,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

const fn events(class: C, project: P) -> F {
    F::ProjectEvents { class, project }
}

use Expect::Allow;

const OWN: F = F::OwnUserActivity;
const FOREIGN: F = F::ForeignUserActivity;
const SHOWN: F = events(C::Integration, P::Private);

/// Every activity row (`matrix::ROWS` appends them).
#[rustfmt::skip]
pub(crate) const ACTIVITY_ROWS: &[Row] = &[
    // --- the route: the person's own ---
    row(W::Reporter, OWN, Allow),
    row(W::NonMember, OWN, Allow),
    row(W::Admin, OWN, Allow),
    row(W::AdminTokenApi, OWN, Allow),
    row(W::OwnerTokenReadApi, OWN, Allow),
    row(W::NonMemberTokenReadApi, OWN, Allow),
    row(W::AgentTokenReadApi, OWN, deny(E::Forbidden)),
    row(W::ServiceAccountTokenReadApi, OWN, deny(E::Forbidden)),
    row(W::BotNonMemberTokenReadApi, OWN, deny(E::Forbidden)),
    row(W::DevTokenReadRepository, OWN, deny(E::InsufficientScope)),
    row(W::DevTokenUserEmail, OWN, deny(E::InsufficientScope)),
    row(W::Anonymous, OWN, deny(E::Unauthenticated)),
    row(W::Disabled, OWN, deny(E::AccountDisabled)),
    // Another user's: not found for everyone.
    row(W::Reporter, FOREIGN, deny(E::NotFound)),
    row(W::Admin, FOREIGN, deny(E::NotFound)),
    row(W::AdminTokenAdmin, FOREIGN, deny(E::NotFound)),
    row(W::ServiceAccountTokenReadApi, FOREIGN, deny(E::NotFound)),
    // --- one listed event: Reporter and above, the integration class ---
    row(W::Reporter, SHOWN, Allow),
    row(W::Developer, SHOWN, Allow),
    row(W::Maintainer, SHOWN, Allow),
    row(W::Owner, SHOWN, Allow),
    row(W::Admin, SHOWN, Allow),
    row(W::OwnerTokenReadApi, SHOWN, Allow),
    row(W::Reporter, events(C::Integration, P::Internal), Allow),
    row(W::Guest, SHOWN, deny(E::Forbidden)),
    row(W::GuestTokenReadApi, SHOWN, deny(E::Forbidden)),
    // Reading an internal project is not being involved in it.
    row(W::NonMember, events(C::Integration, P::Internal), deny(E::Forbidden)),
    row(W::NonMemberTokenReadApi, events(C::Integration, P::Internal), deny(E::Forbidden)),
    // A private project without a role: not found (its name never shows).
    row(W::NonMember, SHOWN, deny(E::NotFound)),
    row(W::AdminTokenApi, SHOWN, deny(E::NotFound)),
    // Never another class, whatever the role.
    row(W::Maintainer, events(C::Maintainer, P::Private), deny(E::Forbidden)),
    row(W::Owner, events(C::Maintainer, P::Private), deny(E::Forbidden)),
    row(W::Owner, events(C::Deployment, P::Private), deny(E::Forbidden)),
    row(W::Admin, events(C::Maintainer, P::Private), deny(E::Forbidden)),
    row(W::Admin, events(C::Admin, P::Private), deny(E::Forbidden)),
    row(W::Reporter, events(C::Integration, P::Deleted), deny(E::PendingDeletion)),
    // People only.
    row(W::AgentTokenReadApi, SHOWN, deny(E::Forbidden)),
    row(W::ServiceAccountTokenReadApi, SHOWN, deny(E::Forbidden)),
    row(W::BotNonMemberTokenReadApi, events(C::Integration, P::Internal), deny(E::Forbidden)),
    row(W::DevTokenReadRepository, SHOWN, deny(E::InsufficientScope)),
    row(W::Disabled, SHOWN, deny(E::AccountDisabled)),
    row(W::Anonymous, SHOWN, deny(E::Unauthenticated)),
];
