//! The permission matrix rows of the push mirror to GitLab (M3 O2, plan 11
//! §5 O2, ADR-0024 Amendment O2): reading and managing a project's mirror
//! are administrators' (a mirror sends the whole repository out of the
//! instance), people only (an agent's token, a service account and a bot
//! are refused, the twins of the administrator's session and `admin` token
//! here), never acting for a person:
//!
//! - `PushMirrorRead` (`GET …/push-mirror`): `read_api` on a token, with
//!   `admin` (a token without it holds no administrator rights: a project
//!   it is no member of is not found);
//! - `PushMirrorManage` (`PUT|DELETE …/push-mirror`, `POST …/sync`): `api`
//!   with `admin`. Not session only (`gstn mirror` uses a token).
//!
//! Owners and Maintainers of the project are refused (the destination is
//! the operator's GitLab, not the project's); pending deletion is refused,
//! administrators included. A destination outside `mirror.allowed_hosts`
//! is not a decision of `authorize()`: forge-core's mirror policy refuses
//! it after the decision (`FG-INT-0021`, forge-core `mirror_tests.rs` and
//! forge-web's `push_mirror_routes.rs`).

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

/// The rows of one mirror action (both are decided alike; the scope each
/// needs is pinned by the rows of `AdminTokenMergeApproveAdmin` and the
/// action's `required_scope`).
macro_rules! mirror {
    ($a:expr) => {
        [
            row($a, W::Admin, F::PrivateProject, Allow),
            row($a, W::AdminTokenAdmin, F::PrivateProject, Allow),
            row($a, W::Admin, F::InternalProject, Allow),
            row($a, W::Owner, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::OwnerTokenApi, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::Maintainer, F::PrivateProject, deny(E::Forbidden)),
            row(
                $a,
                W::MaintainerTokenApi,
                F::PrivateProject,
                deny(E::Forbidden),
            ),
            row($a, W::Developer, F::PrivateProject, deny(E::Forbidden)),
            row($a, W::NonMember, F::InternalProject, deny(E::Forbidden)),
            row($a, W::NonMember, F::PrivateProject, deny(E::NotFound)),
            // No `admin` scope: no administrator rights, and no role.
            row($a, W::AdminTokenApi, F::PrivateProject, deny(E::NotFound)),
            row(
                $a,
                W::AdminTokenMergeApproveAdmin,
                F::PrivateProject,
                deny(E::InsufficientScope),
            ),
            row(
                $a,
                W::AgentAdminTokenAdmin,
                F::PrivateProject,
                deny(E::Forbidden),
            ),
            row(
                $a,
                W::ServiceAccountAdminTokenAdmin,
                F::PrivateProject,
                deny(E::Forbidden),
            ),
            row(
                $a,
                W::BotAdminTokenAdmin,
                F::PrivateProject,
                deny(E::Forbidden),
            ),
            row(
                $a,
                W::ServiceAccountAdminSession,
                F::PrivateProject,
                deny(E::Forbidden),
            ),
            row(
                $a,
                W::BotAdminSession,
                F::PrivateProject,
                deny(E::Forbidden),
            ),
            row($a, W::Admin, F::DeletedProject, deny(E::PendingDeletion)),
            row(
                $a,
                W::DisabledAdmin,
                F::PrivateProject,
                deny(E::AccountDisabled),
            ),
            row(
                $a,
                W::Anonymous,
                F::PrivateProject,
                deny(E::Unauthenticated),
            ),
        ]
    };
}

#[rustfmt::skip]
const READ: [Row; 20] = mirror!(A::PushMirrorRead);
#[rustfmt::skip]
const MANAGE: [Row; 20] = mirror!(A::PushMirrorManage);

/// A `read_api` + `admin` token reads the mirror but does not change it.
#[rustfmt::skip]
const SCOPES: [Row; 2] = [
    row(A::PushMirrorRead, W::AdminTokenReadApiAdmin, F::PrivateProject, Allow),
    row(A::PushMirrorManage, W::AdminTokenReadApiAdmin, F::PrivateProject, deny(E::InsufficientScope)),
];

const GROUPS: [&[Row]; 3] = [&READ, &MANAGE, &SCOPES];

/// Every push mirror row (`matrix::ROWS` appends them).
pub(crate) static MIRROR_ROWS: std::sync::LazyLock<Vec<Row>> =
    std::sync::LazyLock::new(|| GROUPS.concat());
