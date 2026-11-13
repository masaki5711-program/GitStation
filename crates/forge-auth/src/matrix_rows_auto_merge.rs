//! The permission matrix rows of auto-merge (M4 C9, plan 04 M-5,
//! ADR-0017 Amendments 2026-10-30 E and 2026-11-13):
//!
//! - The auto-merge principal (`W::AutoMerge`) may add a merge request to
//!   the merge queue and the landing may merge its entry — on
//!   `Resource::AutoMerge` only, while the project's `merge.auto` is
//!   `conditional` / `full` and no emergency stop is on. It is refused
//!   every other action ([`generated_denies`]): it never approves, never
//!   deploys, never changes protected-branch rules, never toggles an
//!   emergency stop, never reads.
//! - Nobody else reaches `Resource::AutoMerge` (`auto_merge_resource`).
//! - `AutoMergeSettingsManage` (the mode): people only, a session only,
//!   Owner. `AutoMergeStopToggle` (the project's emergency stop): people
//!   only, Maintainer; `api`. `AutoMergeInstanceStopToggle`: people only,
//!   administrators.
//!
//! Service account and bot rows of the people-only actions are in
//! `matrix_rows_people.rs`; CI and act-as denies are generated there.

use forge_core::auto_merge::{AutoMergeMode as M, StopScope as S};
use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::matrix_review::MrFixture as MF;

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

const fn target(mode: M, stop: Option<S>) -> F {
    F::AutoMerge {
        mode,
        stop,
        deleted: false,
    }
}

const FULL: F = target(M::Full, None);

/// The two actions it may perform, and when.
macro_rules! principal_rows {
    ($action:expr) => {
        [
            row($action, W::AutoMerge, FULL, Allow),
            row($action, W::AutoMerge, target(M::Conditional, None), Allow),
            row(
                $action,
                W::AutoMerge,
                target(M::Off, None),
                deny(E::Forbidden),
            ),
            row(
                $action,
                W::AutoMerge,
                target(M::Reserve, None),
                deny(E::Forbidden),
            ),
            row(
                $action,
                W::AutoMerge,
                target(M::Full, Some(S::Project)),
                deny(E::Forbidden),
            ),
            row(
                $action,
                W::AutoMerge,
                target(M::Full, Some(S::Instance)),
                deny(E::Forbidden),
            ),
            row(
                $action,
                W::AutoMerge,
                F::AutoMerge {
                    mode: M::Full,
                    stop: None,
                    deleted: true,
                },
                deny(E::PendingDeletion),
            ),
            // A person's merge request is no auto-merge target.
            row(
                $action,
                W::AutoMerge,
                F::Mr(MF::other()),
                deny(E::Forbidden),
            ),
            // Nobody else reaches the auto-merge target.
            row($action, W::Admin, FULL, deny(E::Forbidden)),
            row($action, W::Maintainer, FULL, deny(E::Forbidden)),
            row($action, W::AgentTokenMergeExecute, FULL, deny(E::Forbidden)),
        ]
    };
}

const ENQUEUE: [Row; 11] = principal_rows!(A::MergeQueueEnqueue);
const MERGE: [Row; 11] = principal_rows!(A::MergeRequestMerge);

#[rustfmt::skip]
const SETTINGS: [Row; 9] = [
    row(A::AutoMergeSettingsManage, W::Owner, F::PrivateProject, Allow),
    row(A::AutoMergeSettingsManage, W::Admin, F::PrivateProject, Allow),
    row(A::AutoMergeSettingsManage, W::Maintainer, F::PrivateProject, deny(E::Forbidden)),
    row(A::AutoMergeSettingsManage, W::OwnerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::AutoMergeSettingsManage, W::AgentOwnerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::AutoMergeSettingsManage, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::AutoMergeSettingsManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::AutoMergeSettingsManage, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::AutoMergeSettingsManage, W::Owner, F::DeletedProject, deny(E::PendingDeletion)),
];

#[rustfmt::skip]
const STOP: [Row; 11] = [
    row(A::AutoMergeStopToggle, W::Maintainer, F::PrivateProject, Allow),
    row(A::AutoMergeStopToggle, W::Owner, F::PrivateProject, Allow),
    row(A::AutoMergeStopToggle, W::Admin, F::PrivateProject, Allow),
    row(A::AutoMergeStopToggle, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::AutoMergeStopToggle, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::AutoMergeStopToggle, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::AutoMergeStopToggle, W::OwnerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::AutoMergeStopToggle, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::AutoMergeStopToggle, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::AutoMergeStopToggle, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::AutoMergeStopToggle, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
];

#[rustfmt::skip]
const INSTANCE_STOP: [Row; 9] = [
    row(A::AutoMergeInstanceStopToggle, W::Admin, F::Instance, Allow),
    row(A::AutoMergeInstanceStopToggle, W::AdminTokenAdmin, F::Instance, Allow),
    row(A::AutoMergeInstanceStopToggle, W::AdminTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::AutoMergeInstanceStopToggle, W::AgentAdminTokenAdmin, F::Instance, deny(E::Forbidden)),
    row(A::AutoMergeInstanceStopToggle, W::Owner, F::Instance, deny(E::Forbidden)),
    row(A::AutoMergeInstanceStopToggle, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    row(A::AutoMergeInstanceStopToggle, W::DisabledAdmin, F::Instance, deny(E::AccountDisabled)),
    row(A::AutoMergeInstanceStopToggle, W::AutoMerge, F::Instance, deny(E::Forbidden)),
    row(A::AutoMergeInstanceStopToggle, W::Maintainer, F::Instance, deny(E::Forbidden)),
];

/// Every explicit row of auto-merge (`matrix::ROWS` appends them).
pub(crate) static AUTO_MERGE_ROWS: std::sync::LazyLock<Vec<Row>> = std::sync::LazyLock::new(|| {
    [ENQUEUE.as_slice(), &MERGE, &SETTINGS, &STOP, &INSTANCE_STOP].concat()
});

/// The auto-merge principal is refused every action it may not perform,
/// on a project and on its own target alike (`reason=auto_merge_not_allowed`;
/// the act-as confirmations answer `FG-REPO-0005` as for everyone they
/// refuse).
pub(crate) fn generated_denies() -> Vec<Row> {
    A::ALL
        .iter()
        .filter(|a| !crate::auto_merge::auto_merge_may(**a))
        .flat_map(|&a| {
            let code = if matches!(a, A::ActAsConfirmView | A::ActAsAssertionIssue) {
                E::NotFound
            } else {
                E::Forbidden
            };
            [
                row(a, W::AutoMerge, F::PrivateProject, deny(code)),
                row(a, W::AutoMerge, FULL, deny(code)),
            ]
        })
        .collect()
}

/// The resource of a [`F::AutoMerge`] row.
#[cfg(test)]
pub(crate) fn target_resource(mode: M, stop: Option<S>, deleted: bool) -> crate::model::Resource {
    let name = |s: &str| forge_core::RefName::parse(s).expect("fixture ref names are valid");
    crate::model::Resource::AutoMerge(crate::model::AutoMergeTarget {
        project: crate::model::ProjectView {
            visibility: crate::model::Visibility::Private,
            role: None,
            pending_deletion: deleted,
        },
        source_branch: name("refs/heads/feature"),
        target_branch: name("refs/heads/main"),
        mode,
        stop,
    })
}
