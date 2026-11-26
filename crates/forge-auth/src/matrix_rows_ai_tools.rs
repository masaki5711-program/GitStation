//! The permission matrix rows of kanflow's tool pack list (plan 19 §3.3,
//! plan 02 §6 F-18, L5-9: `GET /api/v1/ai-tools`, `AiToolList` on the
//! instance): open like the cross search — any active signed-in user, an
//! agent and a service account with `read_api`; anonymous, disabled and
//! narrower tokens are refused. Acting for a person and CI credentials: the
//! generated rows (`ActAsRule::Denied`, `CiRule::Denied`); a portal
//! credential: its own generated rows.
//!
//! The list is static data. Each tool is a route of its own and decided
//! there (`MergeRequestRead`, `MergeRequestList`, `PipelineRead`,
//! `DeployRead`), acting for the person who asked.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

const fn row(actor: W, expect: Expect) -> Row {
    Row {
        action: A::AiToolList,
        actor,
        fixture: F::Instance,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

/// Every tool pack list row (`matrix::ROWS` appends them).
#[rustfmt::skip]
pub(crate) const AI_TOOL_LIST_ROWS: &[Row] = &[
    row(W::NonMember, Allow),
    row(W::Guest, Allow),
    row(W::Admin, Allow),
    row(W::OwnerTokenReadApi, Allow),
    row(W::AgentTokenReadApi, Allow),
    row(W::ServiceAccountTokenReadApi, Allow),
    row(W::Anonymous, deny(E::Unauthenticated)),
    row(W::Disabled, deny(E::AccountDisabled)),
    row(W::DisabledAdmin, deny(E::AccountDisabled)),
    row(W::DevTokenReadRepository, deny(E::InsufficientScope)),
    row(W::DevTokenUserEmail, deny(E::InsufficientScope)),
];
