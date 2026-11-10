//! The permission matrix rows of the document lint's migration setting
//! (plan 13 Q13, ADR-0026 "決まったこと"): lowering a repository's broken
//! document references to warnings until a date is a governance setting,
//! decided like the merge settings — people only (an agent's token here;
//! service accounts and bots in `matrix_rows_people.rs`), Maintainer and
//! above, `api`, never acting for a person (the generated act-as row).
//! Reading it is `ProjectRead` (its rows are the base matrix's).
//!
//! The expiry's bounds (required, in the future, at most 180 days ahead)
//! are not a decision of `authorize()`: the route refuses them after the
//! decision (`FG-WEB-0006`, forge-web's `doc_lint_migration_routes.rs`).

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
const MANAGE: [Row; 15] = [
    row(A::DocLintMigrationManage, W::Maintainer, F::PrivateProject, Allow),
    row(A::DocLintMigrationManage, W::Owner, F::PrivateProject, Allow),
    row(A::DocLintMigrationManage, W::Admin, F::PrivateProject, Allow),
    row(A::DocLintMigrationManage, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::DocLintMigrationManage, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::DocLintMigrationManage, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::DocLintMigrationManage, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(A::DocLintMigrationManage, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::DocLintMigrationManage, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::DocLintMigrationManage, W::OwnerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::DocLintMigrationManage, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::DocLintMigrationManage, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::DocLintMigrationManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::DocLintMigrationManage, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::DocLintMigrationManage, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
];

/// Every row of the setting (`matrix::ROWS` appends them).
pub(crate) const DOC_LINT_ROWS: &[Row] = &MANAGE;
