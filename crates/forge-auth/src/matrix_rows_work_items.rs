//! The permission matrix rows of a person's work item links (M3 I1b, plan
//! 11 §4.3 `WorkItemLink`): Developer and above on the merge request's
//! project, any state, `api`; agents', service accounts' and bots' tokens
//! with that role too (not kept for people: detection links the same from
//! titles). An invisible project is not found, pending deletion refused.
//! A project without an integration binding is the command's
//! (`FG-INT-0018`, forge-web `work_item_link_on_an_unbound_project_is_409`).

use forge_core::MergeRequestState as S;
use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};
use crate::matrix_review::{MrFixture as MF, MrProject as MP};

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

const OTHER_MR: F = F::Mr(MF::other());
const OWN_MR: F = F::Mr(MF::own());

const fn other_in(state: S) -> F {
    F::Mr(MF {
        state,
        ..MF::other()
    })
}

const fn other_on(project: MP) -> F {
    F::Mr(MF {
        project,
        ..MF::other()
    })
}

#[rustfmt::skip]
pub(crate) const WORK_ITEM_ROWS: &[Row] = &[
    row(A::WorkItemLink, W::Developer, OTHER_MR, Allow),
    row(A::WorkItemLink, W::Maintainer, OTHER_MR, Allow),
    row(A::WorkItemLink, W::Owner, OTHER_MR, Allow),
    row(A::WorkItemLink, W::Admin, OTHER_MR, Allow),
    row(A::WorkItemLink, W::DevTokenApi, OTHER_MR, Allow),
    row(A::WorkItemLink, W::AgentTokenApi, OTHER_MR, Allow),
    row(A::WorkItemLink, W::ServiceAccountTokenApi, OTHER_MR, Allow),
    row(A::WorkItemLink, W::BotTokenApi, OTHER_MR, Allow),
    // Links stay after the end: a merged or closed one may get its link.
    row(A::WorkItemLink, W::Developer, other_in(S::Merged), Allow),
    row(A::WorkItemLink, W::Developer, other_in(S::Closed), Allow),
    row(A::WorkItemLink, W::Developer, other_on(MP::Internal), Allow),
    // Below Developer, the author included (plan 11 §4.3).
    row(A::WorkItemLink, W::Reporter, OTHER_MR, deny(E::Forbidden)),
    row(A::WorkItemLink, W::Reporter, OWN_MR, deny(E::Forbidden)),
    row(A::WorkItemLink, W::Guest, OTHER_MR, deny(E::Forbidden)),
    row(A::WorkItemLink, W::ServiceAccountGuestTokenRead, OTHER_MR, deny(E::InsufficientScope)),
    row(A::WorkItemLink, W::NonMember, OTHER_MR, deny(E::NotFound)),
    row(A::WorkItemLink, W::NonMember, other_on(MP::Internal), deny(E::Forbidden)),
    row(A::WorkItemLink, W::Anonymous, OTHER_MR, deny(E::Unauthenticated)),
    row(A::WorkItemLink, W::Disabled, OTHER_MR, deny(E::AccountDisabled)),
    row(A::WorkItemLink, W::OwnerTokenReadApi, OTHER_MR, deny(E::InsufficientScope)),
    row(A::WorkItemLink, W::DevTokenWriteRepository, OTHER_MR, deny(E::InsufficientScope)),
    row(A::WorkItemLink, W::Developer, other_on(MP::Deleted), deny(E::PendingDeletion)),
    row(A::WorkItemLink, W::Admin, other_on(MP::Deleted), deny(E::PendingDeletion)),
];
