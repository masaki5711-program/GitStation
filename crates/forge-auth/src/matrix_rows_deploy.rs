//! The permission matrix rows of deployments (M4 C11, plan 14 §4.2,
//! ADR-0031): reading (Reporter, as `RepoBrowse`), environments
//! (Maintainer, people only), targets (administrators, people only),
//! requests (Developer) and the approval (people only, Maintainer, never
//! the requester nor the merge request's author, prod in a web session
//! only). The service account and bot twins of the people-only actions
//! are in `matrix_rows_people.rs`; the runner, job and act-as denies are
//! generated (`matrix_rows_ci.rs`, `matrix_rows_act_as.rs`).

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

use Expect::Allow;

/// A deployment to approve: prod or stg, requested by the caller or by
/// another person, from a merge request of the caller or none.
const fn dep(prod: bool, mine: bool, mr_mine: bool, project: P) -> F {
    F::Deployment {
        prod,
        requested_by_me: mine,
        mr_author_me: mr_mine,
        project,
    }
}
pub(crate) const STG: F = dep(false, false, false, P::Private);
const PROD: F = dep(true, false, false, P::Private);
const PROD_MINE: F = dep(true, true, false, P::Private);
const PROD_MR_MINE: F = dep(true, false, true, P::Private);
const STG_MINE: F = dep(false, true, false, P::Private);
const PROD_INTERNAL: F = dep(true, false, false, P::Internal);
const PROD_DELETED: F = dep(true, false, false, P::Deleted);

/// Every deployment row (`matrix::ROWS` appends them).
#[rustfmt::skip]
pub(crate) const DEPLOY_ROWS: &[Row] = &[
    // --- DeployRead: as RepoBrowse ---
    row(A::DeployRead, W::Reporter, F::PrivateProject, Allow),
    row(A::DeployRead, W::Developer, F::PrivateProject, Allow),
    row(A::DeployRead, W::AgentTokenReadApi, F::PrivateProject, Allow),
    row(A::DeployRead, W::NonMember, F::InternalProject, Allow),
    row(A::DeployRead, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployRead, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::DeployRead, W::DevTokenUserEmail, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::DeployRead, W::Developer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::DeployRead, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::DeployRead, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    // --- DeployEnvironmentManage: Maintainer, people only ---
    row(A::DeployEnvironmentManage, W::Maintainer, F::PrivateProject, Allow),
    row(A::DeployEnvironmentManage, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::DeployEnvironmentManage, W::Owner, F::PrivateProject, Allow),
    row(A::DeployEnvironmentManage, W::Admin, F::PrivateProject, Allow),
    row(A::DeployEnvironmentManage, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployEnvironmentManage, W::DevTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployEnvironmentManage, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::DeployEnvironmentManage, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::DeployEnvironmentManage, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::DeployEnvironmentManage, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::DeployEnvironmentManage, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployEnvironmentManage, W::AgentOwnerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployEnvironmentManage, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::DeployEnvironmentManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    // --- DeployTargetManage: administrators, people only ---
    row(A::DeployTargetManage, W::Admin, F::PrivateProject, Allow),
    row(A::DeployTargetManage, W::AdminTokenAdmin, F::PrivateProject, Allow),
    row(A::DeployTargetManage, W::Owner, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployTargetManage, W::Maintainer, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployTargetManage, W::OwnerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    // An administrator's token without `admin` is a person without a role.
    row(A::DeployTargetManage, W::AdminTokenApi, F::PrivateProject, deny(E::NotFound)),
    row(A::DeployTargetManage, W::AgentAdminTokenAdmin, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployTargetManage, W::DisabledAdmin, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::DeployTargetManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    // --- DeployCreate: Developer ---
    row(A::DeployCreate, W::Developer, F::PrivateProject, Allow),
    row(A::DeployCreate, W::DevTokenApi, F::PrivateProject, Allow),
    row(A::DeployCreate, W::AgentTokenApi, F::PrivateProject, Allow),
    row(A::DeployCreate, W::ServiceAccountTokenApi, F::PrivateProject, Allow),
    row(A::DeployCreate, W::Maintainer, F::PrivateProject, Allow),
    row(A::DeployCreate, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployCreate, W::ServiceAccountReporterTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployCreate, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::DeployCreate, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::DeployCreate, W::AgentTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::DeployCreate, W::Developer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::DeployCreate, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    // --- DeployApprove: a web session on every tier ---
    row(A::DeployApprove, W::Maintainer, STG, Allow),
    row(A::DeployApprove, W::Owner, STG, Allow),
    row(A::DeployApprove, W::Admin, STG, Allow),
    row(A::DeployApprove, W::Maintainer, PROD, Allow),
    row(A::DeployApprove, W::Owner, PROD, Allow),
    row(A::DeployApprove, W::Admin, PROD, Allow),
    // A token never approves (session_required), whatever the tier.
    row(A::DeployApprove, W::MaintainerTokenApi, STG, deny(E::InsufficientScope)),
    row(A::DeployApprove, W::MaintainerTokenApi, PROD, deny(E::InsufficientScope)),
    row(A::DeployApprove, W::OwnerTokenApi, PROD, deny(E::InsufficientScope)),
    row(A::DeployApprove, W::AdminTokenAdmin, PROD, deny(E::InsufficientScope)),
    row(A::DeployApprove, W::DevTokenApi, STG, deny(E::InsufficientScope)),
    row(A::DeployApprove, W::MaintainerTokenReadApi, STG, deny(E::InsufficientScope)),
    // Never one's own request nor a change one is behind, administrators
    // included.
    row(A::DeployApprove, W::Maintainer, PROD_MINE, deny(E::DeploySelfApprovalRefused)),
    row(A::DeployApprove, W::Maintainer, PROD_MR_MINE, deny(E::DeploySelfApprovalRefused)),
    row(A::DeployApprove, W::Admin, PROD_MINE, deny(E::DeploySelfApprovalRefused)),
    row(A::DeployApprove, W::Admin, PROD_MR_MINE, deny(E::DeploySelfApprovalRefused)),
    row(A::DeployApprove, W::Maintainer, STG_MINE, deny(E::DeploySelfApprovalRefused)),
    // Below Maintainer.
    row(A::DeployApprove, W::Developer, STG, deny(E::Forbidden)),
    row(A::DeployApprove, W::Developer, PROD, deny(E::Forbidden)),
    row(A::DeployApprove, W::NonMember, PROD_INTERNAL, deny(E::Forbidden)),
    row(A::DeployApprove, W::NonMember, PROD, deny(E::NotFound)),
    // Automation never approves: an agent token (refused as a token), a
    // service account (the kind of the auto-merge principal) or a bot,
    // even in a session.
    row(A::DeployApprove, W::AgentMaintainerTokenApi, STG, deny(E::InsufficientScope)),
    row(A::DeployApprove, W::AgentAdminTokenAdmin, PROD, deny(E::InsufficientScope)),
    row(A::DeployApprove, W::ServiceAccountMaintainerTokenApi, PROD, deny(E::InsufficientScope)),
    row(A::DeployApprove, W::ServiceAccountAdminSession, PROD, deny(E::Forbidden)),
    row(A::DeployApprove, W::BotAdminSession, PROD, deny(E::Forbidden)),
    row(A::DeployApprove, W::Maintainer, PROD_DELETED, deny(E::PendingDeletion)),
    row(A::DeployApprove, W::Disabled, PROD, deny(E::AccountDisabled)),
    row(A::DeployApprove, W::Anonymous, PROD, deny(E::Unauthenticated)),
];
