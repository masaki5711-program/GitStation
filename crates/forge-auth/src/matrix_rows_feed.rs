//! The permission matrix rows of the integrations' event feed (M3 I1c,
//! ADR-0016 §7 "マトリクスの deny の行", plan 02 §11.2):
//!
//! - the routes (`IntegrationEventRead`, `IntegrationEventCursorRead` on
//!   `Fixture::IntegrationFeed`): only the integration's own service
//!   account, `read_api` (`api` satisfies it). Another integration's feed,
//!   a person — an administrator's session or `admin`-scoped token too —
//!   and a bot are the same 404; an agent's token, an act-as request and a
//!   disabled integration are 403; a disabled account `FG-AUTH-0004`.
//! - one bound project's events (`Fixture::ProjectEvents`, decided by
//!   `prove_visible`): the integration class for a Reporter or higher
//!   membership of the service account; the maintainer, deployment and
//!   admin classes never; a Guest, a project it is no member of (404) and
//!   one pending deletion (`FG-REPO-0006`) are not visible; a person is
//!   never the feed's principal.

use forge_core::VisibilityClass as C;
use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};
use crate::matrix_review::MrProject as P;

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

use Expect::Allow;

const OWN: F = F::IntegrationFeed {
    own: true,
    enabled: true,
    act_as: false,
};
const FOREIGN: F = F::IntegrationFeed {
    own: false,
    enabled: true,
    act_as: false,
};
const DISABLED: F = F::IntegrationFeed {
    own: true,
    enabled: false,
    act_as: false,
};
const ACT_AS: F = F::IntegrationFeed {
    own: true,
    enabled: true,
    act_as: true,
};

const fn events(class: C, project: P) -> F {
    F::ProjectEvents { class, project }
}

#[rustfmt::skip]
pub(crate) const FEED_ROWS: &[Row] = &[
    // The feed: its own service account, read_api (api satisfies it).
    row(A::IntegrationEventRead, W::ServiceAccountTokenReadApi, OWN, Allow),
    row(A::IntegrationEventRead, W::ServiceAccountReporterTokenApi, OWN, Allow),
    row(A::IntegrationEventRead, W::ServiceAccountTokenReadApi, FOREIGN, deny(E::NotFound)),
    row(A::IntegrationEventRead, W::ServiceAccountTokenReadApi, DISABLED, deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::ServiceAccountTokenReadApi, ACT_AS, deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::ServiceAccountAgentTokenReadApi, OWN, deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::ServiceAccountDisabledTokenReadApi, OWN, deny(E::AccountDisabled)),
    row(A::IntegrationEventRead, W::ServiceAccountTokenMergeApprove, OWN, deny(E::InsufficientScope)),
    // People (administrators included) and bots: never the feed's caller.
    row(A::IntegrationEventRead, W::Admin, OWN, deny(E::NotFound)),
    row(A::IntegrationEventRead, W::AdminTokenAdmin, OWN, deny(E::NotFound)),
    row(A::IntegrationEventRead, W::Admin, FOREIGN, deny(E::NotFound)),
    row(A::IntegrationEventRead, W::DevTokenApi, OWN, deny(E::NotFound)),
    row(A::IntegrationEventRead, W::AgentTokenReadApi, OWN, deny(E::NotFound)),
    row(A::IntegrationEventRead, W::BotTokenApi, OWN, deny(E::NotFound)),
    row(A::IntegrationEventRead, W::ServiceAccountAdminTokenAdmin, FOREIGN, deny(E::NotFound)),
    row(A::IntegrationEventRead, W::Anonymous, OWN, deny(E::Unauthenticated)),
    // The head cursor: decided the same way.
    row(A::IntegrationEventCursorRead, W::ServiceAccountTokenReadApi, OWN, Allow),
    row(A::IntegrationEventCursorRead, W::ServiceAccountReporterTokenApi, OWN, Allow),
    row(A::IntegrationEventCursorRead, W::ServiceAccountTokenReadApi, FOREIGN, deny(E::NotFound)),
    row(A::IntegrationEventCursorRead, W::ServiceAccountTokenReadApi, DISABLED, deny(E::Forbidden)),
    row(A::IntegrationEventCursorRead, W::ServiceAccountTokenReadApi, ACT_AS, deny(E::Forbidden)),
    row(A::IntegrationEventCursorRead, W::ServiceAccountAgentTokenReadApi, OWN, deny(E::Forbidden)),
    row(A::IntegrationEventCursorRead, W::ServiceAccountDisabledTokenReadApi, OWN, deny(E::AccountDisabled)),
    row(A::IntegrationEventCursorRead, W::Admin, OWN, deny(E::NotFound)),
    row(A::IntegrationEventCursorRead, W::AdminTokenAdmin, OWN, deny(E::NotFound)),
    row(A::IntegrationEventCursorRead, W::BotTokenApi, OWN, deny(E::NotFound)),
    // Review (I1c): the cursor route mirrors every deny of the feed route.
    row(A::IntegrationEventCursorRead, W::ServiceAccountTokenMergeApprove, OWN, deny(E::InsufficientScope)),
    row(A::IntegrationEventCursorRead, W::Admin, FOREIGN, deny(E::NotFound)),
    row(A::IntegrationEventCursorRead, W::DevTokenApi, OWN, deny(E::NotFound)),
    row(A::IntegrationEventCursorRead, W::AgentTokenReadApi, OWN, deny(E::NotFound)),
    row(A::IntegrationEventCursorRead, W::ServiceAccountAdminTokenAdmin, FOREIGN, deny(E::NotFound)),
    row(A::IntegrationEventCursorRead, W::AdminTokenAdmin, FOREIGN, deny(E::NotFound)),
    row(A::IntegrationEventRead, W::AdminTokenAdmin, FOREIGN, deny(E::NotFound)),
    row(A::IntegrationEventCursorRead, W::Anonymous, OWN, deny(E::Unauthenticated)),
    // A bound project's events: the integration class by membership.
    row(A::IntegrationEventRead, W::ServiceAccountReporterTokenApi, events(C::Integration, P::Private), Allow),
    row(A::IntegrationEventRead, W::ServiceAccountTokenReadApi, events(C::Integration, P::Private), Allow),
    row(A::IntegrationEventRead, W::ServiceAccountTokenReadApi, events(C::Integration, P::Internal), Allow),
    row(A::IntegrationEventRead, W::ServiceAccountGuestTokenRead, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::ServiceAccountNonMemberTokenReadApi, events(C::Integration, P::Private), deny(E::NotFound)),
    row(A::IntegrationEventRead, W::ServiceAccountNonMemberTokenReadApi, events(C::Integration, P::Internal), deny(E::NotFound)),
    row(A::IntegrationEventRead, W::ServiceAccountTokenReadApi, events(C::Integration, P::Deleted), deny(E::PendingDeletion)),
    // Review (I1c): administrator rights never stand in for a membership;
    // a Guest of an internal project reads nothing.
    row(A::IntegrationEventRead, W::ServiceAccountAdminTokenAdmin, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::ServiceAccountGuestTokenRead, events(C::Integration, P::Internal), deny(E::Forbidden)),
    row(A::IntegrationEventCursorRead, W::ServiceAccountAdminTokenAdmin, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationEventCursorRead, W::ServiceAccountGuestTokenRead, events(C::Integration, P::Internal), deny(E::Forbidden)),
    // Never the maintainer, deployment or admin class, whatever the role.
    row(A::IntegrationEventRead, W::ServiceAccountOwnerTokenApi, events(C::Maintainer, P::Private), deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::ServiceAccountMaintainerTokenApi, events(C::Maintainer, P::Private), deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::ServiceAccountOwnerTokenApi, events(C::Deployment, P::Private), deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::ServiceAccountOwnerTokenApi, events(C::Admin, P::Private), deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::ServiceAccountAdminTokenAdmin, events(C::Admin, P::Private), deny(E::Forbidden)),
    // Never a person's, an agent's or a bot's.
    row(A::IntegrationEventRead, W::Reporter, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::Admin, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::ServiceAccountAgentTokenReadApi, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::BotTokenApi, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationEventRead, W::DevTokenUserEmail, events(C::Integration, P::Private), deny(E::InsufficientScope)),
    row(A::IntegrationEventRead, W::Anonymous, events(C::Integration, P::Private), deny(E::Unauthenticated)),
    row(A::IntegrationEventCursorRead, W::ServiceAccountReporterTokenApi, events(C::Integration, P::Private), Allow),
    row(A::IntegrationEventCursorRead, W::ServiceAccountOwnerTokenApi, events(C::Maintainer, P::Private), deny(E::Forbidden)),
];
