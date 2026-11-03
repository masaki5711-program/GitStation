//! The permission matrix rows of service accounts (M3 S0, plan 11 §5 S0,
//! ADR-0019 Addendum S0): the administration routes (create and issue are
//! an administrator's web session only; list, read and the token list and
//! revoke take an `admin`-scoped token; a target of another kind is not
//! found), what a service account may not do as a caller (create in its
//! own personal namespace, see an internal project it is no member of),
//! and the `user_email` scope that reads no API. The service account / bot
//! caller rows of the people-only actions are in `matrix_rows_people.rs`.

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

const SA: F = F::ServiceAccountTarget {
    human: false,
    active: true,
};
const SA_DISABLED: F = F::ServiceAccountTarget {
    human: false,
    active: false,
};
const PERSON: F = F::ServiceAccountTarget {
    human: true,
    active: true,
};
const ISSUE: F = F::ServiceAccountTokenRequest {
    human: false,
    active: true,
    admin_scope: false,
};
const ISSUE_DISABLED: F = F::ServiceAccountTokenRequest {
    human: false,
    active: false,
    admin_scope: false,
};
const ISSUE_PERSON: F = F::ServiceAccountTokenRequest {
    human: true,
    active: true,
    admin_scope: false,
};
const ISSUE_ADMIN_SCOPE: F = F::ServiceAccountTokenRequest {
    human: false,
    active: true,
    admin_scope: true,
};

#[rustfmt::skip]
pub(crate) const SERVICE_ACCOUNT_ROWS: &[Row] = &[
    // Create: an administrator's web session only.
    row(A::ServiceAccountCreate, W::Admin, F::Instance, Allow),
    row(A::ServiceAccountCreate, W::AdminTokenAdmin, F::Instance, deny(E::InsufficientScope)),
    row(A::ServiceAccountCreate, W::AgentAdminTokenAdmin, F::Instance, deny(E::InsufficientScope)),
    row(A::ServiceAccountCreate, W::Developer, F::Instance, deny(E::Forbidden)),
    row(A::ServiceAccountCreate, W::DisabledAdmin, F::Instance, deny(E::AccountDisabled)),
    row(A::ServiceAccountCreate, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    // List: administrator rights (a token: `admin` + `read_api`).
    row(A::ServiceAccountList, W::Admin, F::Instance, Allow),
    row(A::ServiceAccountList, W::AdminTokenAdmin, F::Instance, Allow),
    row(A::ServiceAccountList, W::AdminTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::ServiceAccountList, W::DevTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::ServiceAccountList, W::Developer, F::Instance, deny(E::Forbidden)),
    row(A::ServiceAccountList, W::DevTokenWriteRepository, F::Instance, deny(E::InsufficientScope)),
    // Read: administrators; a person under the route is not found.
    row(A::ServiceAccountRead, W::Admin, SA, Allow),
    row(A::ServiceAccountRead, W::Admin, SA_DISABLED, Allow),
    row(A::ServiceAccountRead, W::AdminTokenAdmin, SA, Allow),
    row(A::ServiceAccountRead, W::Admin, PERSON, deny(E::NotFound)),
    row(A::ServiceAccountRead, W::AdminTokenApi, SA, deny(E::Forbidden)),
    row(A::ServiceAccountRead, W::Developer, SA, deny(E::Forbidden)),
    row(A::ServiceAccountRead, W::Developer, PERSON, deny(E::Forbidden)),
    // Update: administrators, people only.
    row(A::ServiceAccountUpdate, W::Admin, SA, Allow),
    row(A::ServiceAccountUpdate, W::AdminTokenAdmin, SA, Allow),
    row(A::ServiceAccountUpdate, W::Admin, SA_DISABLED, Allow),
    row(A::ServiceAccountUpdate, W::Admin, PERSON, deny(E::NotFound)),
    row(A::ServiceAccountUpdate, W::AgentAdminTokenAdmin, SA, deny(E::Forbidden)),
    row(A::ServiceAccountUpdate, W::AdminTokenApi, SA, deny(E::Forbidden)),
    row(A::ServiceAccountUpdate, W::Developer, SA, deny(E::Forbidden)),
    // Issue: an administrator's web session, an active service account,
    // never `admin`; a token (an administrator's included) never issues one.
    row(A::ServiceAccountTokenIssue, W::Admin, ISSUE, Allow),
    row(A::ServiceAccountTokenIssue, W::AdminTokenAdmin, ISSUE, deny(E::InsufficientScope)),
    row(A::ServiceAccountTokenIssue, W::AgentAdminTokenAdmin, ISSUE, deny(E::InsufficientScope)),
    row(A::ServiceAccountTokenIssue, W::Admin, ISSUE_PERSON, deny(E::NotFound)),
    row(A::ServiceAccountTokenIssue, W::Admin, ISSUE_DISABLED, deny(E::Forbidden)),
    row(A::ServiceAccountTokenIssue, W::Admin, ISSUE_ADMIN_SCOPE, deny(E::Forbidden)),
    row(A::ServiceAccountTokenIssue, W::Developer, ISSUE, deny(E::Forbidden)),
    row(A::ServiceAccountTokenIssue, W::DisabledAdmin, ISSUE, deny(E::AccountDisabled)),
    // The token list: administrators (`admin` + `read_api` on a token).
    row(A::ServiceAccountTokenList, W::Admin, SA, Allow),
    row(A::ServiceAccountTokenList, W::AdminTokenAdmin, SA, Allow),
    row(A::ServiceAccountTokenList, W::Admin, PERSON, deny(E::NotFound)),
    row(A::ServiceAccountTokenList, W::AdminTokenApi, SA, deny(E::Forbidden)),
    row(A::ServiceAccountTokenList, W::Developer, SA, deny(E::Forbidden)),
    // Revoke: administrators, an `admin`-scoped token included (an
    // agent's too: revoking only takes access away), a disabled account's.
    row(A::ServiceAccountTokenRevoke, W::Admin, SA, Allow),
    row(A::ServiceAccountTokenRevoke, W::AdminTokenAdmin, SA, Allow),
    row(A::ServiceAccountTokenRevoke, W::AgentAdminTokenAdmin, SA, Allow),
    row(A::ServiceAccountTokenRevoke, W::Admin, SA_DISABLED, Allow),
    row(A::ServiceAccountTokenRevoke, W::Admin, PERSON, deny(E::NotFound)),
    row(A::ServiceAccountTokenRevoke, W::AdminTokenApi, SA, deny(E::Forbidden)),
    row(A::ServiceAccountTokenRevoke, W::Developer, SA, deny(E::Forbidden)),
    row(A::ServiceAccountTokenRevoke, W::AdminTokenMergeApproveAdmin, SA, deny(E::InsufficientScope)),
    // A service account (or a bot) creates nothing in its own personal
    // namespace; in a group it may, like a person of its role.
    row(A::ProjectCreate, W::ServiceAccountOwnerTokenApi, F::OwnNamespace, deny(E::Forbidden)),
    row(A::ProjectCreate, W::BotOwnerTokenApi, F::OwnNamespace, deny(E::Forbidden)),
    row(A::ProjectCreate, W::OwnerTokenApi, F::OwnNamespace, Allow),
    row(A::ProjectCreate, W::ServiceAccountMaintainerTokenApi, F::Group, Allow),
    // S0 review: not by anyone else either — an administrator neither
    // creates nor moves a project into a service account's namespace (its
    // owner would become the project's implicit Owner).
    row(A::ProjectCreate, W::Admin, F::ServiceAccountNamespace, deny(E::Forbidden)),
    row(A::ProjectCreate, W::AdminTokenAdmin, F::ServiceAccountNamespace, deny(E::Forbidden)),
    row(A::ProjectTransfer, W::Admin, F::TransferToServiceAccountNamespace, deny(E::Forbidden)),
    row(A::ProjectTransfer, W::Owner, F::TransferToServiceAccountNamespace, deny(E::Forbidden)),
    // A service account sees member projects only: an internal project it
    // is no member of is not found (a person reads it as a Reporter).
    row(A::ProjectRead, W::ServiceAccountNonMemberTokenReadApi, F::InternalProject, deny(E::NotFound)),
    row(A::ProjectRead, W::NonMemberTokenReadApi, F::InternalProject, Allow),
    row(A::ProjectRead, W::ServiceAccountTokenApi, F::PrivateProject, Allow),
    row(A::ProjectRead, W::ServiceAccountTokenApi, F::InternalProject, Allow),
    row(A::RepoBrowse, W::ServiceAccountNonMemberTokenReadApi, F::InternalProject, deny(E::NotFound)),
    row(A::PathResolve, W::ServiceAccountNonMemberTokenReadApi, F::ResolvedInternal, deny(E::NotFound)),
    row(A::PathResolve, W::ServiceAccountNonMemberTokenReadApi, F::ResolvedGroup, Allow),
    row(A::MergeRequestList, W::ServiceAccountNonMemberTokenReadApi, F::InternalProject, deny(E::NotFound)),
    // `user_email` adds the addresses to what a token reads; alone it
    // reads nothing.
    row(A::ProjectRead, W::DevTokenUserEmail, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::UserSelfRead, W::DevTokenUserEmail, F::Instance, deny(E::InsufficientScope)),
];
