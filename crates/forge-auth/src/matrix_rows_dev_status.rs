//! The permission matrix rows of the development status (M3 I1d,
//! `IntegrationDevStatusRead`: `GET /api/v1/integrations/{id}/work-items/{ref}/dev-status`
//! and `GET /api/v1/integrations/{id}/dev-status`, plan 02 §5.2, §11.2).
//!
//! The development status is decided exactly like the event feed
//! (`matrix_rows_feed.rs`) and on the same visible set (`prove_visible`):
//! every `IntegrationEventRead` row has its twin here with the same actor,
//! fixture and outcome (`the_dev_status_routes_mirror_every_feed_row`).
//!
//! - the routes (`Fixture::IntegrationFeed`): only the integration's own
//!   service account, `read_api` (`api` satisfies it). Another
//!   integration's, a person's (an administrator's session or `admin`
//!   token too) and a bot's call are the same 404; an agent's token, an
//!   act-as request and a disabled integration 403; a disabled account
//!   `FG-AUTH-0004`.
//! - one bound project (`Fixture::ProjectEvents`): the integration class
//!   by a Reporter or higher membership of the service account, never the
//!   maintainer, deployment or admin class; a Guest, a non-member (404) and
//!   a project pending deletion (`FG-REPO-0006`) are not visible.

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
pub(crate) const DEV_STATUS_ROWS: &[Row] = &[
    // The routes: the integration's own service account only.
    row(A::IntegrationDevStatusRead, W::ServiceAccountTokenReadApi, OWN, Allow),
    row(A::IntegrationDevStatusRead, W::ServiceAccountReporterTokenApi, OWN, Allow),
    row(A::IntegrationDevStatusRead, W::ServiceAccountTokenReadApi, FOREIGN, deny(E::NotFound)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountTokenReadApi, DISABLED, deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountTokenReadApi, ACT_AS, deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountAgentTokenReadApi, OWN, deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountDisabledTokenReadApi, OWN, deny(E::AccountDisabled)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountTokenMergeApprove, OWN, deny(E::InsufficientScope)),
    row(A::IntegrationDevStatusRead, W::Admin, OWN, deny(E::NotFound)),
    row(A::IntegrationDevStatusRead, W::AdminTokenAdmin, OWN, deny(E::NotFound)),
    row(A::IntegrationDevStatusRead, W::Admin, FOREIGN, deny(E::NotFound)),
    row(A::IntegrationDevStatusRead, W::DevTokenApi, OWN, deny(E::NotFound)),
    row(A::IntegrationDevStatusRead, W::AgentTokenReadApi, OWN, deny(E::NotFound)),
    row(A::IntegrationDevStatusRead, W::BotTokenApi, OWN, deny(E::NotFound)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountAdminTokenAdmin, FOREIGN, deny(E::NotFound)),
    row(A::IntegrationDevStatusRead, W::Anonymous, OWN, deny(E::Unauthenticated)),
    row(A::IntegrationDevStatusRead, W::AdminTokenAdmin, FOREIGN, deny(E::NotFound)),
    // A bound project: the feed's visible set.
    row(A::IntegrationDevStatusRead, W::ServiceAccountReporterTokenApi, events(C::Integration, P::Private), Allow),
    row(A::IntegrationDevStatusRead, W::ServiceAccountTokenReadApi, events(C::Integration, P::Private), Allow),
    row(A::IntegrationDevStatusRead, W::ServiceAccountTokenReadApi, events(C::Integration, P::Internal), Allow),
    row(A::IntegrationDevStatusRead, W::ServiceAccountGuestTokenRead, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountNonMemberTokenReadApi, events(C::Integration, P::Private), deny(E::NotFound)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountNonMemberTokenReadApi, events(C::Integration, P::Internal), deny(E::NotFound)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountTokenReadApi, events(C::Integration, P::Deleted), deny(E::PendingDeletion)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountAdminTokenAdmin, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountGuestTokenRead, events(C::Integration, P::Internal), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountOwnerTokenApi, events(C::Maintainer, P::Private), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountMaintainerTokenApi, events(C::Maintainer, P::Private), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountOwnerTokenApi, events(C::Deployment, P::Private), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountOwnerTokenApi, events(C::Admin, P::Private), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountAdminTokenAdmin, events(C::Admin, P::Private), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::Reporter, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::Admin, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::ServiceAccountAgentTokenReadApi, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::BotTokenApi, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::IntegrationDevStatusRead, W::DevTokenUserEmail, events(C::Integration, P::Private), deny(E::InsufficientScope)),
    row(A::IntegrationDevStatusRead, W::Anonymous, events(C::Integration, P::Private), deny(E::Unauthenticated)),
];
