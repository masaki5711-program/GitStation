//! The permission matrix rows of the cross search (plan 20 K3, ADR-0040,
//! `GET /api/v1/search`, `SearchQuery` on the instance): open like the
//! project list — any active signed-in user, an agent and a service
//! account with `read_api`; anonymous, disabled and narrower tokens are
//! refused. Acting for a person and CI credentials: the generated rows
//! (`ActAsRule::Denied`, `CiRule::Denied`); a portal credential: its own
//! generated rows.
//!
//! Every hit is then decided again with the existing actions —
//! `ProjectRead` for a project, `MergeRequestList` on the hit's project for
//! a merge request — whose rows are the base matrix's (a private project
//! without a role is not found, a Guest reads no merge request, a project
//! pending deletion is refused). The route's tests prove that a hit is
//! never shown before that decision allowed it.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

const fn row(actor: W, expect: Expect) -> Row {
    Row {
        action: A::SearchQuery,
        actor,
        fixture: F::Instance,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

/// Every search row (`matrix::ROWS` appends them).
#[rustfmt::skip]
pub(crate) const SEARCH_QUERY_ROWS: &[Row] = &[
    row(W::NonMember, Allow),
    row(W::Guest, Allow),
    row(W::Developer, Allow),
    row(W::Admin, Allow),
    row(W::OwnerTokenReadApi, Allow),
    row(W::NonMemberTokenReadApi, Allow),
    row(W::DevTokenApi, Allow),
    // An agent searches too; confidential hits are withheld after the
    // decision (`is_agent_audience`), never counted.
    row(W::AgentTokenReadApi, Allow),
    row(W::ServiceAccountTokenReadApi, Allow),
    row(W::Anonymous, deny(E::Unauthenticated)),
    row(W::Disabled, deny(E::AccountDisabled)),
    row(W::DisabledAdmin, deny(E::AccountDisabled)),
    row(W::DevTokenReadRepository, deny(E::InsufficientScope)),
    row(W::DevTokenWriteRepository, deny(E::InsufficientScope)),
    row(W::DevTokenUserEmail, deny(E::InsufficientScope)),
];
