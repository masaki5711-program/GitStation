//! The permission matrix rows of webhooks (M3 I2, ADR-0016 §8, plan 11
//! §4.3 "WebhookManage / DeliveryRead / Redeliver: Maintainer 以上 /
//! Developer は不可"):
//!
//! - reading a project's webhooks and their deliveries: Maintainer and
//!   above, `read_api` (an agent's token of a Maintainer reads too);
//! - managing them and asking for a delivery again: people only (an
//!   agent's token is refused; the service account / bot twins are in
//!   `matrix_rows_people.rs`), Maintainer and above, `api`;
//! - a delivery (`WebhookDeliver` on `Fixture::ProjectEvents`, decided by
//!   `prove_webhook_delivery` for the webhook's owner): Maintainer of the
//!   project, the integration, maintainer and deployment classes, never
//!   the admin class; pending deletion refused, an invisible project not
//!   found.
//!
//! A target outside the operator's allowlist (plan 11's other deny
//! example) is not a decision of `authorize()`: forge-core's target policy
//! refuses it after the decision (`FG-INT-0010`, forge-core
//! `webhook_tests.rs` and forge-web's route tests).

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

const fn events(class: C, project: P) -> F {
    F::ProjectEvents { class, project }
}

use Expect::Allow;

/// The read rows of one read action.
macro_rules! reads {
    ($a:expr) => {
        [
            row($a, W::Maintainer, F::PrivateProject, Allow),
            row($a, W::MaintainerTokenReadApi, F::PrivateProject, Allow),
            row($a, W::MaintainerTokenApi, F::PrivateProject, Allow),
            row($a, W::Owner, F::PrivateProject, Allow),
            row($a, W::Admin, F::PrivateProject, Allow),
            row($a, W::Maintainer, F::InternalProject, Allow),
            row($a, W::AgentMaintainerTokenApi, F::PrivateProject, Allow),
            row(
                $a,
                W::ServiceAccountMaintainerTokenApi,
                F::PrivateProject,
                Allow,
            ),
            row($a, W::Developer, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::DevTokenApi, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::Guest, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::NonMember, F::InternalProject, deny(E::Forbidden)),
            row($a, W::NonMember, F::PrivateProject, deny(E::NotFound)),
            row($a, W::AdminTokenApi, F::PrivateProject, deny(E::NotFound)),
            row(
                $a,
                W::DevTokenReadRepository,
                F::PrivateProject,
                deny(E::InsufficientScope),
            ),
            row(
                $a,
                W::Maintainer,
                F::DeletedProject,
                deny(E::PendingDeletion),
            ),
            row($a, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
            row(
                $a,
                W::Anonymous,
                F::PrivateProject,
                deny(E::Unauthenticated),
            ),
        ]
    };
}

/// The write rows of one people-only write action.
macro_rules! writes {
    ($a:expr) => {
        [
            row($a, W::Maintainer, F::PrivateProject, Allow),
            row($a, W::MaintainerTokenApi, F::PrivateProject, Allow),
            row($a, W::Owner, F::PrivateProject, Allow),
            row($a, W::OwnerTokenApi, F::PrivateProject, Allow),
            row($a, W::Admin, F::PrivateProject, Allow),
            row($a, W::Maintainer, F::InternalProject, Allow),
            row($a, W::Developer, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::DevTokenApi, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::NonMember, F::InternalProject, deny(E::Forbidden)),
            row($a, W::NonMember, F::PrivateProject, deny(E::NotFound)),
            row(
                $a,
                W::Maintainer,
                F::DeletedProject,
                deny(E::PendingDeletion),
            ),
            row($a, W::Admin, F::DeletedProject, deny(E::PendingDeletion)),
            row(
                $a,
                W::MaintainerTokenReadApi,
                F::PrivateProject,
                deny(E::InsufficientScope),
            ),
            row(
                $a,
                W::AgentMaintainerTokenApi,
                F::PrivateProject,
                deny(E::Forbidden),
            ),
            row(
                $a,
                W::AgentOwnerTokenApi,
                F::PrivateProject,
                deny(E::Forbidden),
            ),
            row($a, W::AdminTokenApi, F::PrivateProject, deny(E::NotFound)),
            row($a, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
            row(
                $a,
                W::Anonymous,
                F::PrivateProject,
                deny(E::Unauthenticated),
            ),
        ]
    };
}

const READ: [Row; 19] = reads!(A::WebhookRead);
const DELIVERY_READ: [Row; 19] = reads!(A::WebhookDeliveryRead);
const MANAGE: [Row; 19] = writes!(A::WebhookManage);
const REDELIVER: [Row; 19] = writes!(A::WebhookRedeliver);

#[rustfmt::skip]
const DELIVER: [Row; 16] = [
    row(A::WebhookDeliver, W::Maintainer, events(C::Integration, P::Private), Allow),
    row(A::WebhookDeliver, W::Maintainer, events(C::Maintainer, P::Private), Allow),
    row(A::WebhookDeliver, W::Maintainer, events(C::Deployment, P::Private), Allow),
    row(A::WebhookDeliver, W::Owner, events(C::Integration, P::Private), Allow),
    row(A::WebhookDeliver, W::Admin, events(C::Integration, P::Private), Allow),
    row(A::WebhookDeliver, W::Maintainer, events(C::Integration, P::Internal), Allow),
    // The admin class is never a webhook's, administrators included.
    row(A::WebhookDeliver, W::Maintainer, events(C::Admin, P::Private), deny(E::Forbidden)),
    row(A::WebhookDeliver, W::Admin, events(C::Admin, P::Private), deny(E::Forbidden)),
    // The owner lost the project (owner_lost).
    row(A::WebhookDeliver, W::Developer, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::WebhookDeliver, W::Reporter, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::WebhookDeliver, W::NonMember, events(C::Integration, P::Internal), deny(E::Forbidden)),
    row(A::WebhookDeliver, W::NonMember, events(C::Integration, P::Private), deny(E::NotFound)),
    row(A::WebhookDeliver, W::Maintainer, events(C::Integration, P::Deleted), deny(E::PendingDeletion)),
    row(A::WebhookDeliver, W::Disabled, events(C::Integration, P::Private), deny(E::AccountDisabled)),
    row(A::WebhookDeliver, W::AgentMaintainerTokenApi, events(C::Integration, P::Private), deny(E::Forbidden)),
    row(A::WebhookDeliver, W::Anonymous, events(C::Integration, P::Private), deny(E::Unauthenticated)),
];

const GROUPS: [&[Row]; 5] = [&READ, &DELIVERY_READ, &MANAGE, &REDELIVER, &DELIVER];

/// Every webhook row (`matrix::ROWS` appends them).
pub(crate) static WEBHOOK_ROWS: std::sync::LazyLock<Vec<Row>> =
    std::sync::LazyLock::new(|| GROUPS.concat());
