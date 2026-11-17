//! The permission matrix rows of the deterministic autofix (M4 C14,
//! ADR-0017 §2.5 and its Amendment C14):
//!
//! - The autofix bot (`W::Autofix`) may push (`RepoPush`) the one commit it
//!   built to the source branch of the merge request it was asked to fix —
//!   on `Resource::Autofix` only: a fast-forward of an unprotected branch
//!   of an open merge request, autofix enabled, the author and the enabler
//!   active Developers or above. Each condition has its deny row.
//! - It is refused every other action ([`generated_denies`]): it never
//!   approves, merges, queues, reserves, changes a protected ref or a
//!   setting, deploys, acts as someone, or reads.
//! - Nobody else reaches `Resource::Autofix` (`autofix_resource`).
//! - `AutofixSettingsManage` (the project's switch): people only,
//!   Maintainer; `api`. Service account and bot rows are in
//!   `matrix_rows_people.rs`.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

/// The cases of the bot's target (each breaks one condition but `Ok`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Case {
    Ok,
    Protected,
    NotFastForward,
    Closed,
    Disabled,
    AuthorReporter,
    AuthorInactive,
    EnablerLostRole,
    SourceIsTarget,
    Deleted,
}

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

const OK: F = F::Autofix(Case::Ok);

#[rustfmt::skip]
const PUSH: [Row; 14] = [
    row(A::RepoPush, W::Autofix, OK, Allow),
    row(A::RepoPush, W::Autofix, F::Autofix(Case::Protected), deny(E::Forbidden)),
    row(A::RepoPush, W::Autofix, F::Autofix(Case::NotFastForward), deny(E::Forbidden)),
    row(A::RepoPush, W::Autofix, F::Autofix(Case::Closed), deny(E::Forbidden)),
    row(A::RepoPush, W::Autofix, F::Autofix(Case::Disabled), deny(E::Forbidden)),
    row(A::RepoPush, W::Autofix, F::Autofix(Case::AuthorReporter), deny(E::Forbidden)),
    row(A::RepoPush, W::Autofix, F::Autofix(Case::AuthorInactive), deny(E::Forbidden)),
    row(A::RepoPush, W::Autofix, F::Autofix(Case::EnablerLostRole), deny(E::Forbidden)),
    row(A::RepoPush, W::Autofix, F::Autofix(Case::SourceIsTarget), deny(E::Forbidden)),
    row(A::RepoPush, W::Autofix, F::Autofix(Case::Deleted), deny(E::PendingDeletion)),
    // A plain project push is no autofix target: the bot pushes nothing else.
    row(A::RepoPush, W::Autofix, F::PrivateProject, deny(E::Forbidden)),
    // Nobody else reaches the autofix target, an administrator included.
    row(A::RepoPush, W::Admin, OK, deny(E::Forbidden)),
    row(A::RepoPush, W::Maintainer, OK, deny(E::Forbidden)),
    row(A::RepoPush, W::AutoMerge, OK, deny(E::Forbidden)),
];

#[rustfmt::skip]
const SETTINGS: [Row; 12] = [
    row(A::AutofixSettingsManage, W::Maintainer, F::PrivateProject, Allow),
    row(A::AutofixSettingsManage, W::Owner, F::PrivateProject, Allow),
    row(A::AutofixSettingsManage, W::Admin, F::PrivateProject, Allow),
    row(A::AutofixSettingsManage, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::AutofixSettingsManage, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::AutofixSettingsManage, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::AutofixSettingsManage, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::AutofixSettingsManage, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::AutofixSettingsManage, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::AutofixSettingsManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::AutofixSettingsManage, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::AutofixSettingsManage, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
];

/// Every explicit row of autofix (`matrix::ROWS` appends them).
pub(crate) static AUTOFIX_ROWS: std::sync::LazyLock<Vec<Row>> =
    std::sync::LazyLock::new(|| [PUSH.as_slice(), &SETTINGS].concat());

/// The bot is refused every action it may not perform, on a project and on
/// its own target alike (`reason=autofix_not_allowed`: approving, merging,
/// queueing, protected refs and paths, settings, deploying, acting as
/// someone, reading; the act-as confirmations answer `FG-REPO-0005` as for
/// everyone they refuse).
pub(crate) fn generated_denies() -> Vec<Row> {
    A::ALL
        .iter()
        .filter(|a| !crate::autofix::autofix_may(**a))
        .flat_map(|&a| {
            let code = if matches!(a, A::ActAsConfirmView | A::ActAsAssertionIssue) {
                E::NotFound
            } else {
                E::Forbidden
            };
            [
                row(a, W::Autofix, F::PrivateProject, deny(code)),
                row(a, W::Autofix, OK, deny(code)),
            ]
        })
        .collect()
}

/// The resource of a [`F::Autofix`] row.
#[cfg(test)]
pub(crate) fn target_resource(case: Case) -> crate::model::Resource {
    use crate::model::{
        AutofixTarget, ProjectView, RefProtection, RefUpdate, Resource, Role, Visibility,
    };
    let name = |s: &str| forge_core::RefName::parse(s).expect("fixture ref names are valid");
    let mut t = AutofixTarget {
        project: ProjectView {
            visibility: Visibility::Private,
            role: None,
            pending_deletion: false,
        },
        source_branch: name("refs/heads/feature"),
        target_branch: name("refs/heads/main"),
        update: RefUpdate::FastForward,
        protection: None,
        mr_open: true,
        autofix_enabled: true,
        author_role: Some(Role::Developer),
        author_active: true,
        enabler_role: Some(Role::Maintainer),
        enabler_active: true,
    };
    match case {
        Case::Ok => {}
        Case::Protected => t.protection = RefProtection::effective(Some(Role::Developer)),
        Case::NotFastForward => t.update = RefUpdate::Force,
        Case::Closed => t.mr_open = false,
        Case::Disabled => t.autofix_enabled = false,
        Case::AuthorReporter => t.author_role = Some(Role::Reporter),
        Case::AuthorInactive => t.author_active = false,
        Case::EnablerLostRole => t.enabler_role = None,
        Case::SourceIsTarget => t.source_branch = name("refs/heads/main"),
        Case::Deleted => t.project.pending_deletion = true,
    }
    Resource::Autofix(t)
}
