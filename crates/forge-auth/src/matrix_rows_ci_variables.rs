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
    Row {
        action: A::CiVariableManage,
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
