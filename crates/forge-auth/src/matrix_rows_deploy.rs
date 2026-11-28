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
use crate::matrix::{Actor as W, DeployBehalf as B, Expect, Fixture as F, Row};
use crate::matrix_review::MrProject as P;
use crate::matrix_rows_portal::Case as PC;
use crate::model::ApproverGate as G;
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

use Expect::Allow;

/// A deployment to approve: prod or stg, requested by the caller or by
/// another person, from a merge request of the caller or none.
const fn dep(prod: bool, mine: bool, mr_mine: bool, project: P) -> F {
    F::Deployment {
        prod,
        requested_by_me: mine,
        mr_author_me: mr_mine,
        project,
        approvers: G::Unrestricted,
        behalf: B::Own,
    }
}

/// ADR-0038 §3.4: a prod deployment of an environment that names
/// approvers, the caller matching them as `gate`.
const fn listed(gate: G) -> F {
    F::Deployment {
        prod: true,
        requested_by_me: false,
        mr_author_me: false,
        project: P::Private,
        approvers: gate,
        behalf: B::Own,
    }
}

/// ADR-0038 §3.5 (review of step C): a deployment approved for a
/// delegator who holds `role` on the project now, the caller matching the
/// environment's approvers as `gate`.
const fn for_delegator(prod: bool, gate: G, role: Option<R>) -> F {
    F::Deployment {
        prod,
        requested_by_me: false,
        mr_author_me: false,
        project: P::Private,
        approvers: gate,
        behalf: B::Delegator(role),
    }
}
/// A delegation named that does not hold (review of ADR-0038 step C).
const fn refused(behalf: B) -> F {
    F::Deployment {
        prod: true,
        requested_by_me: false,
        mr_author_me: false,
        project: P::Private,
        approvers: G::Named,
        behalf,
    }
}
/// ADR-0038 §3.5: approving as the delegate of the requester.
const PROD_FOR_REQUESTER: F = F::Deployment {
    prod: true,
    requested_by_me: false,
    mr_author_me: false,
    project: P::Private,
    approvers: G::Named,
    behalf: B::Requester,
};
const NAMED: F = listed(G::Named);
const MATCHED: F = listed(G::Matched);
const NOT_LISTED: F = listed(G::NotListed("not_listed"));
const DELEGATION_OFF: F = listed(G::NotListed("delegation_off"));
pub(crate) const STG: F = dep(false, false, false, P::Private);
pub(crate) const PROD: F = dep(true, false, false, P::Private);
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
    // ADR-0038 §3.4: an environment with approvers — named people from
    // Reporter, groups and selectors from Maintainer, nobody else (an
    // Owner and an administrator included).
    row(A::DeployApprove, W::Reporter, NAMED, Allow),
    row(A::DeployApprove, W::Developer, NAMED, Allow),
    row(A::DeployApprove, W::Guest, NAMED, deny(E::Forbidden)),
    row(A::DeployApprove, W::Maintainer, MATCHED, Allow),
    row(A::DeployApprove, W::Developer, MATCHED, deny(E::Forbidden)),
    row(A::DeployApprove, W::Reporter, MATCHED, deny(E::Forbidden)),
    row(A::DeployApprove, W::Maintainer, NOT_LISTED, deny(E::DeployApproverNotListed)),
    row(A::DeployApprove, W::Owner, NOT_LISTED, deny(E::DeployApproverNotListed)),
    row(A::DeployApprove, W::Admin, NOT_LISTED, deny(E::DeployApproverNotListed)),
    row(A::DeployApprove, W::Guest, NOT_LISTED, deny(E::Forbidden)),
    row(A::DeployApprove, W::Maintainer, DELEGATION_OFF, deny(E::DeployApproverNotListed)),
    row(A::DeployApprove, W::MaintainerTokenApi, NAMED, deny(E::InsufficientScope)),
    row(A::DeployApprove, W::BotAdminSession, NAMED, deny(E::Forbidden)),
    // ADR-0038 §3.5: a delegate never approves the delegator's own request.
    row(A::DeployApprove, W::Maintainer, PROD_FOR_REQUESTER, deny(E::DeploySelfApprovalRefused)),
    // The delegator needs the role the gate asks of the caller (review of
    // ADR-0038 step C), prod and stg alike; the caller's administrator
    // bypass is not lent to them.
    row(A::DeployApprove, W::Reporter, for_delegator(true, G::Named, Some(R::Reporter)), Allow),
    row(A::DeployApprove, W::Reporter, for_delegator(true, G::Named, None), deny(E::Forbidden)),
    row(A::DeployApprove, W::Reporter, for_delegator(true, G::Named, Some(R::Guest)), deny(E::Forbidden)),
    row(A::DeployApprove, W::Maintainer, for_delegator(true, G::Matched, Some(R::Maintainer)), Allow),
    row(A::DeployApprove, W::Maintainer, for_delegator(true, G::Matched, Some(R::Developer)), deny(E::Forbidden)),
    row(A::DeployApprove, W::Maintainer, for_delegator(false, G::Matched, None), deny(E::Forbidden)),
    row(A::DeployApprove, W::Admin, for_delegator(true, G::Matched, None), deny(E::Forbidden)),
    row(A::DeployApprove, W::Maintainer, for_delegator(false, G::Unrestricted, Some(R::Developer)), deny(E::Forbidden)),
    // A named delegation that does not hold (or a name that does not
    // resolve) is a loaded fact: told only to someone who may approve.
    row(A::DeployApprove, W::Maintainer, refused(B::NoDelegation), deny(E::ApprovalDelegationInactive)),
    row(A::DeployApprove, W::Reporter, refused(B::UnknownDelegator), deny(E::ApprovalDelegationInactive)),
    row(A::DeployApprove, W::Guest, refused(B::NoDelegation), deny(E::Forbidden)),
    row(A::DeployApprove, W::Guest, refused(B::UnknownDelegator), deny(E::Forbidden)),
    row(A::DeployApprove, W::MaintainerTokenApi, refused(B::NoDelegation), deny(E::InsufficientScope)),
    // Widget design P2b: the PFS portal's credential never approves — the
    // `deploy-approvals` widget only shows what the person could approve
    // in a session (`portal_not_allowed`, an administrator's account too).
    row(A::DeployApprove, W::Portal(PC::Reporter), PROD, deny(E::Forbidden)),
    row(A::DeployApprove, W::Portal(PC::Reporter), STG, deny(E::Forbidden)),
    row(A::DeployApprove, W::Portal(PC::Reporter), NAMED, deny(E::Forbidden)),
    row(A::DeployApprove, W::Portal(PC::Admin), PROD, deny(E::Forbidden)),
    // --- DeployApproversManage: Owner, people only ---
    row(A::DeployApproversManage, W::Owner, F::PrivateProject, Allow),
    row(A::DeployApproversManage, W::OwnerTokenApi, F::PrivateProject, Allow),
    row(A::DeployApproversManage, W::Admin, F::PrivateProject, Allow),
    row(A::DeployApproversManage, W::Maintainer, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployApproversManage, W::MaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployApproversManage, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployApproversManage, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::DeployApproversManage, W::Owner, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::DeployApproversManage, W::AgentOwnerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::DeployApproversManage, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::DeployApproversManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    // --- ApprovalDelegationRead / Manage (ADR-0038 §3.5): one's own only;
    // setting it is a person's, in a session ---
    row(A::ApprovalDelegationRead, W::Reporter, F::OwnToken, Allow),
    row(A::ApprovalDelegationRead, W::OwnerTokenReadApi, F::OwnToken, Allow),
    row(A::ApprovalDelegationRead, W::Admin, F::ForeignToken, deny(E::NotFound)),
    row(A::ApprovalDelegationRead, W::DevTokenReadRepository, F::OwnToken, deny(E::InsufficientScope)),
    row(A::ApprovalDelegationRead, W::Anonymous, F::OwnToken, deny(E::Unauthenticated)),
    row(A::ApprovalDelegationManage, W::Reporter, F::OwnToken, Allow),
    row(A::ApprovalDelegationManage, W::Admin, F::OwnToken, Allow),
    row(A::ApprovalDelegationManage, W::DevTokenApi, F::OwnToken, deny(E::InsufficientScope)),
    row(A::ApprovalDelegationManage, W::Admin, F::ForeignToken, deny(E::NotFound)),
    row(A::ApprovalDelegationManage, W::Disabled, F::OwnToken, deny(E::AccountDisabled)),
    row(A::ApprovalDelegationManage, W::Anonymous, F::OwnToken, deny(E::Unauthenticated)),
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
