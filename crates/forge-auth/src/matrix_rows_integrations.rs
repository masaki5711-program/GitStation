//! The permission matrix rows of integrations (M3 I1a, plan 11 §4.3, §5
//! I1): managing one is an administrator's web session only (a token, an
//! `admin`-scoped one included, never), reading them administrators'
//! (people only; a token needs `admin` + `read_api`), binding a project a
//! Maintainer's (people only; `api`; pending deletion refused, an invisible
//! project not found). The service account / bot twin rows of the three
//! people-only actions are in `matrix_rows_people.rs`.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};

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

#[rustfmt::skip]
pub(crate) const INTEGRATION_ROWS: &[Row] = &[
    // Manage (create, enable / disable, event types): an administrator's
    // web session only.
    row(A::IntegrationManage, W::Admin, F::Instance, Allow),
    row(A::IntegrationManage, W::AdminTokenAdmin, F::Instance, deny(E::InsufficientScope)),
    row(A::IntegrationManage, W::AgentAdminTokenAdmin, F::Instance, deny(E::InsufficientScope)),
    row(A::IntegrationManage, W::AdminTokenApi, F::Instance, deny(E::InsufficientScope)),
    row(A::IntegrationManage, W::Owner, F::Instance, deny(E::Forbidden)),
    row(A::IntegrationManage, W::Developer, F::Instance, deny(E::Forbidden)),
    row(A::IntegrationManage, W::DisabledAdmin, F::Instance, deny(E::AccountDisabled)),
    row(A::IntegrationManage, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    // Read: administrators, people only; a token needs `admin` + `read_api`.
    row(A::IntegrationRead, W::Admin, F::Instance, Allow),
    row(A::IntegrationRead, W::AdminTokenAdmin, F::Instance, Allow),
    row(A::IntegrationRead, W::AdminTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::IntegrationRead, W::AgentAdminTokenAdmin, F::Instance, deny(E::Forbidden)),
    row(A::IntegrationRead, W::AdminTokenMergeApproveAdmin, F::Instance, deny(E::InsufficientScope)),
    row(A::IntegrationRead, W::Developer, F::Instance, deny(E::Forbidden)),
    row(A::IntegrationRead, W::DevTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::IntegrationRead, W::DisabledAdmin, F::Instance, deny(E::AccountDisabled)),
    row(A::IntegrationRead, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    // Bind / unbind a project: Maintainer and above, people only, `api`.
    row(A::IntegrationBindingManage, W::Maintainer, F::PrivateProject, Allow),
    row(A::IntegrationBindingManage, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::IntegrationBindingManage, W::Owner, F::PrivateProject, Allow),
    row(A::IntegrationBindingManage, W::OwnerTokenApi, F::PrivateProject, Allow),
    row(A::IntegrationBindingManage, W::Admin, F::PrivateProject, Allow),
    row(A::IntegrationBindingManage, W::Maintainer, F::InternalProject, Allow),
    row(A::IntegrationBindingManage, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::IntegrationBindingManage, W::DevTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::IntegrationBindingManage, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::IntegrationBindingManage, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::IntegrationBindingManage, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::IntegrationBindingManage, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::IntegrationBindingManage, W::Admin, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::IntegrationBindingManage, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::IntegrationBindingManage, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::IntegrationBindingManage, W::AgentOwnerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::IntegrationBindingManage, W::AdminTokenApi, F::PrivateProject, deny(E::NotFound)),
    row(A::IntegrationBindingManage, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::IntegrationBindingManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
];
