//! The permission matrix rows of the merge queue (M4 C8, plan 14 §4.2,
//! ADR-0030 §2, §9–§11):
//!
//! - `MergeQueueRead` (the queue, one merge request's place): as
//!   `RepoBrowse` — an agent audience too (metadata only); `read_api`.
//! - `MergeQueueEnqueue` / `MergeQueueDequeue` on the merge request: as
//!   `MergeRequestMerge` — people only, the target's minimum push role
//!   (Developer when unprotected), a token needs `merge_execute`.
//! - `MergeQueuePause` (the emergency stop): people only, Developer and
//!   above; `MergeQueueManage` (resume): people only, Maintainer and above;
//!   both `api`.
//!
//! Service account and bot rows of the people-only actions are in
//! `matrix_rows_people.rs`; a runner and a job are refused every one
//! (`matrix_rows_ci::generated_denies`), and none is performed acting for a
//! person (`matrix_rows_act_as::generated_denies`).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::matrix_review::{MrFixture as MF, MrProject as MP};
use crate::model::Role as R;

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

const OTHER_MR: F = F::Mr(MF::other());

const fn protected(role: R) -> F {
    F::Mr(MF {
        protected: Some(role),
        ..MF::other()
    })
}

const fn mr_on(project: MP) -> F {
    F::Mr(MF {
        project,
        ..MF::other()
    })
}

#[rustfmt::skip]
const READ: [Row; 13] = [
    row(A::MergeQueueRead, W::Reporter, F::PrivateProject, Allow),
    row(A::MergeQueueRead, W::Developer, F::PrivateProject, Allow),
    row(A::MergeQueueRead, W::Admin, F::PrivateProject, Allow),
    row(A::MergeQueueRead, W::NonMember, F::InternalProject, Allow),
    row(A::MergeQueueRead, W::AgentTokenReadApi, F::PrivateProject, Allow),
    row(A::MergeQueueRead, W::ServiceAccountTokenApi, F::PrivateProject, Allow),
    row(A::MergeQueueRead, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeQueueRead, W::DevTokenReadRepository, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::MergeQueueRead, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::MergeQueueRead, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::MergeQueueRead, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::MergeQueueRead, W::Reporter, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::MergeQueueRead, W::OwnerTokenReadApi, F::PrivateProject, Allow),
];

/// Enqueue and dequeue: the rows of `MergeRequestMerge` (the same arm).
macro_rules! merge_rows {
    ($action:expr) => {
        [
            row($action, W::Developer, OTHER_MR, Allow),
            row($action, W::Developer, F::Mr(MF::own()), Allow),
            row($action, W::Maintainer, protected(R::Maintainer), Allow),
            row($action, W::Admin, protected(R::Owner), Allow),
            row($action, W::DevTokenMergeExecute, OTHER_MR, Allow),
            row(
                $action,
                W::Developer,
                protected(R::Maintainer),
                deny(E::Forbidden),
            ),
            row(
                $action,
                W::DevTokenMergeExecute,
                protected(R::Maintainer),
                deny(E::Forbidden),
            ),
            row(
                $action,
                W::AgentTokenMergeExecute,
                OTHER_MR,
                deny(E::Forbidden),
            ),
            row(
                $action,
                W::DevTokenApi,
                OTHER_MR,
                deny(E::InsufficientScope),
            ),
            row(
                $action,
                W::AgentTokenApi,
                OTHER_MR,
                deny(E::InsufficientScope),
            ),
            row($action, W::Reporter, OTHER_MR, deny(E::Forbidden)),
            row(
                $action,
                W::NonMember,
                mr_on(MP::Internal),
                deny(E::Forbidden),
            ),
            row($action, W::NonMember, OTHER_MR, deny(E::NotFound)),
            row($action, W::Anonymous, OTHER_MR, deny(E::Unauthenticated)),
            row($action, W::Disabled, OTHER_MR, deny(E::AccountDisabled)),
            row(
                $action,
                W::Developer,
                mr_on(MP::Deleted),
                deny(E::PendingDeletion),
            ),
        ]
    };
}

#[rustfmt::skip]
const ENQUEUE: [Row; 16] = merge_rows!(A::MergeQueueEnqueue);
#[rustfmt::skip]
const DEQUEUE: [Row; 16] = merge_rows!(A::MergeQueueDequeue);

#[rustfmt::skip]
const PAUSE: [Row; 12] = [
    row(A::MergeQueuePause, W::Developer, F::PrivateProject, Allow),
    row(A::MergeQueuePause, W::Maintainer, F::PrivateProject, Allow),
    row(A::MergeQueuePause, W::Admin, F::PrivateProject, Allow),
    row(A::MergeQueuePause, W::DevTokenApi, F::PrivateProject, Allow),
    row(A::MergeQueuePause, W::AgentTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeQueuePause, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeQueuePause, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::MergeQueuePause, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::MergeQueuePause, W::OwnerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::MergeQueuePause, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::MergeQueuePause, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::MergeQueuePause, W::Developer, F::DeletedProject, deny(E::PendingDeletion)),
];

#[rustfmt::skip]
const RESUME: [Row; 11] = [
    row(A::MergeQueueManage, W::Maintainer, F::PrivateProject, Allow),
    row(A::MergeQueueManage, W::Admin, F::PrivateProject, Allow),
    row(A::MergeQueueManage, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::MergeQueueManage, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeQueueManage, W::DevTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeQueueManage, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeQueueManage, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::MergeQueueManage, W::OwnerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::MergeQueueManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::MergeQueueManage, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::MergeQueueManage, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
];

/// Every row of the merge queue (`matrix::ROWS` appends them).
pub(crate) static MERGE_QUEUE_ROWS: std::sync::LazyLock<Vec<Row>> =
    std::sync::LazyLock::new(|| [READ.as_slice(), &ENQUEUE, &DEQUEUE, &PAUSE, &RESUME].concat());
