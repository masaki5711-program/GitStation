//! The permission matrix rows of CI pipelines (M4 C2, plan 14 §4.2):
//! reading a project's pipelines is decided like `RepoBrowse` (an agent
//! audience included: the routes answer metadata only until C5 decides the
//! CI outputs on their marks, ADR-0029 decision 5); running one by hand,
//! cancelling and retrying need Developer and above by membership — an
//! agent's token and a service account included (plan 14 §4.2: AI-driven
//! development runs and retries its own pipelines). A runner and a job are
//! refused every one (`matrix_rows_ci::generated_denies`), and no action
//! is performed acting for a person (`matrix_rows_act_as`).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

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

#[rustfmt::skip]
const READ: [Row; 14] = [
    row(A::PipelineRead, W::Reporter, F::PrivateProject, Allow),
    row(A::PipelineRead, W::Developer, F::PrivateProject, Allow),
    row(A::PipelineRead, W::Admin, F::PrivateProject, Allow),
    row(A::PipelineRead, W::NonMember, F::InternalProject, Allow),
    row(A::PipelineRead, W::AgentTokenReadApi, F::PrivateProject, Allow),
    row(A::PipelineRead, W::ServiceAccountTokenApi, F::PrivateProject, Allow),
    row(A::PipelineRead, W::OwnerTokenReadApi, F::PrivateProject, Allow),
    row(A::PipelineRead, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(A::PipelineRead, W::GuestTokenReadApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::PipelineRead, W::DevTokenReadRepository, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::PipelineRead, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::PipelineRead, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::PipelineRead, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::PipelineRead, W::Reporter, F::DeletedProject, deny(E::PendingDeletion)),
];

/// Developer and above for each change (the same shape for the three).
macro_rules! developer_rows {
    ($action:expr) => {
        [
            row($action, W::Developer, F::PrivateProject, Allow),
            row($action, W::Maintainer, F::PrivateProject, Allow),
            row($action, W::Admin, F::PrivateProject, Allow),
            row($action, W::DevTokenApi, F::PrivateProject, Allow),
            row($action, W::AgentTokenApi, F::PrivateProject, Allow),
            row($action, W::ServiceAccountTokenApi, F::PrivateProject, Allow),
            row($action, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
            row($action, W::Guest, F::PrivateProject, deny(E::Forbidden)),
            row(
                $action,
                W::NonMember,
                F::InternalProject,
                deny(E::Forbidden),
            ),
            row($action, W::NonMember, F::PrivateProject, deny(E::NotFound)),
            row(
                $action,
                W::AgentTokenReadApi,
                F::PrivateProject,
                deny(E::InsufficientScope),
            ),
            row(
                $action,
                W::OwnerTokenReadApi,
                F::PrivateProject,
                deny(E::InsufficientScope),
            ),
            row(
                $action,
                W::Anonymous,
                F::PrivateProject,
                deny(E::Unauthenticated),
            ),
            row(
                $action,
                W::Disabled,
                F::PrivateProject,
                deny(E::AccountDisabled),
            ),
            row(
                $action,
                W::Developer,
                F::DeletedProject,
                deny(E::PendingDeletion),
            ),
        ]
    };
}

const CREATE: [Row; 15] = developer_rows!(A::PipelineCreate);
const CANCEL: [Row; 15] = developer_rows!(A::PipelineCancel);
const RETRY: [Row; 15] = developer_rows!(A::PipelineRetry);

/// Every row of the pipelines (`matrix::ROWS` appends them).
pub(crate) static PIPELINE_ROWS: std::sync::LazyLock<Vec<Row>> =
    std::sync::LazyLock::new(|| [READ.as_slice(), &CREATE, &CANCEL, &RETRY].concat());
