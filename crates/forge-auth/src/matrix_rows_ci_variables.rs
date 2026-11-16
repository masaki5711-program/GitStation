//! The permission matrix rows of CI variables (M4 C6, plan 14 §4.2
//! "CiVariableManage: Maintainer 以上、人だけ / Developer、エージェント、
//! サービスアカウントは不可"): listing, setting and deleting a project's
//! CI secrets is a Maintainer's, people only, `api`. An agent's token is
//! refused here; the service account and bot twins are in
//! `matrix_rows_people.rs`, the runner and job denies are generated
//! (`ci_denied_for_ci_variable_manage`, `matrix_rows_ci.rs`), the act-as
//! deny by `ActAsRule::Denied`.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};

const fn row(actor: W, fixture: F, expect: Expect) -> Row {
    on(A::CiVariableManage, actor, fixture, expect)
}

const fn on(action: A, actor: W, fixture: F, expect: Expect) -> Row {
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

/// Every CI variable row (`matrix::ROWS` appends them).
#[rustfmt::skip]
pub(crate) const CI_VARIABLE_ROWS: &[Row] = &[
    row(W::Maintainer, F::PrivateProject, Allow),
    row(W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(W::Owner, F::PrivateProject, Allow),
    row(W::OwnerTokenApi, F::PrivateProject, Allow),
    row(W::Admin, F::PrivateProject, Allow),
    row(W::Maintainer, F::InternalProject, Allow),
    row(W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(W::DevTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
    row(W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(W::AgentOwnerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(W::AdminTokenApi, F::PrivateProject, deny(E::NotFound)),
    row(W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
];

/// M4 C12: pipeline schedules — the same shape (Maintainer, people only,
/// `api`): a schedule runs with its owner's rights, so only a person may
/// become an owner. The service account and bot twins are in
/// `matrix_rows_people.rs`; runner, job and act-as denies are generated.
#[rustfmt::skip]
pub(crate) const SCHEDULE_ROWS: &[Row] = &[
    on(A::PipelineScheduleManage, W::Maintainer, F::PrivateProject, Allow),
    on(A::PipelineScheduleManage, W::MaintainerTokenApi, F::PrivateProject, Allow),
    on(A::PipelineScheduleManage, W::Owner, F::PrivateProject, Allow),
    on(A::PipelineScheduleManage, W::Admin, F::PrivateProject, Allow),
    on(A::PipelineScheduleManage, W::Maintainer, F::InternalProject, Allow),
    on(A::PipelineScheduleManage, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    on(A::PipelineScheduleManage, W::DevTokenApi, F::PrivateProject, deny(E::Forbidden)),
    on(A::PipelineScheduleManage, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    on(A::PipelineScheduleManage, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    on(A::PipelineScheduleManage, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    on(A::PipelineScheduleManage, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
    on(A::PipelineScheduleManage, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    on(A::PipelineScheduleManage, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    on(A::PipelineScheduleManage, W::AgentOwnerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    on(A::PipelineScheduleManage, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    on(A::PipelineScheduleManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
];

/// M4 C12 review: a manual job of a **protected** pipeline (its jobs get
/// the project's secrets) is started by a Maintainer, people only; on an
/// unprotected pipeline the route's `PipelineCreate` rows hold (Developer,
/// agents and service accounts by their membership).
#[rustfmt::skip]
pub(crate) const PLAY_PROTECTED_ROWS: &[Row] = &[
    on(A::PipelinePlayProtected, W::Maintainer, F::PrivateProject, Allow),
    on(A::PipelinePlayProtected, W::MaintainerTokenApi, F::PrivateProject, Allow),
    on(A::PipelinePlayProtected, W::Owner, F::PrivateProject, Allow),
    on(A::PipelinePlayProtected, W::Admin, F::PrivateProject, Allow),
    on(A::PipelinePlayProtected, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    on(A::PipelinePlayProtected, W::DevTokenApi, F::PrivateProject, deny(E::Forbidden)),
    on(A::PipelinePlayProtected, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    on(A::PipelinePlayProtected, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    on(A::PipelinePlayProtected, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
    on(A::PipelinePlayProtected, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    on(A::PipelinePlayProtected, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    on(A::PipelinePlayProtected, W::AgentOwnerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    on(A::PipelinePlayProtected, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    on(A::PipelinePlayProtected, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
];
