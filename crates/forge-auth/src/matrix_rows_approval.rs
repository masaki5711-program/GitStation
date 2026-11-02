//! The permission matrix rows of approvals and approval rules (M3 R5,
//! ADR-0024 §8.3–§8.6, §12).
//!
//! - `MergeRequestApprove` / `MergeRequestUnapprove` on the merge request:
//!   people only (an agent's token `agent_token` here; service accounts and
//!   bots in `matrix_rows_people.rs`), Developer and above by membership,
//!   never the author — administrators included (`FG-REVIEW-0013`,
//!   `admin_author_self_approval_is_denied`). A token needs
//!   `merge_approve`: `api`, `admin` and `merge_execute` never satisfy it
//!   (`FG-AUTH-0002`). Readiness and whether the approval counts are
//!   `evaluate_merge()`'s: a draft or merged merge request is no denial.
//! - The R4 debt: a `merge_approve`-only token on `MergeRequestMerge` is
//!   denied (`FG-AUTH-0002`).
//! - `ApprovalRuleManage` on the project: people only, Maintainer and
//!   above, `api`.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::matrix_review::{MrBranches as MB, MrFixture as MF, MrProject as MP};
use forge_core::MergeRequestState as S;

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

const OTHER_MR: F = F::Mr(MF::other());
const OWN_MR: F = F::Mr(MF::own());

const fn mr_on(project: MP) -> F {
    F::Mr(MF {
        project,
        ..MF::other()
    })
}

/// The rows of one approval action (`MergeRequestApprove` and
/// `MergeRequestUnapprove` are decided alike).
macro_rules! approval_rows {
    ($a:expr) => {
        [
            // --- allowed: a person, Developer and above, not the author ---
            row($a, W::Developer, OTHER_MR, Allow),
            row($a, W::Maintainer, OTHER_MR, Allow),
            row($a, W::Owner, OTHER_MR, Allow),
            row($a, W::Admin, OTHER_MR, Allow),
            row($a, W::DevTokenMergeApprove, OTHER_MR, Allow),
            // Readiness is evaluate_merge()'s.
            row(
                $a,
                W::Developer,
                F::Mr(MF {
                    draft: true,
                    ..MF::other()
                }),
                Allow,
            ),
            row(
                $a,
                W::Developer,
                F::Mr(MF {
                    state: S::Merged,
                    ..MF::other()
                }),
                Allow,
            ),
            // --- the author: never, administrators included ---
            row($a, W::Developer, OWN_MR, deny(E::SelfApprovalRefused)),
            row($a, W::Admin, OWN_MR, deny(E::SelfApprovalRefused)),
            row(
                $a,
                W::DevTokenMergeApprove,
                OWN_MR,
                deny(E::SelfApprovalRefused),
            ),
            // --- people only (agents here; service accounts and bots in matrix_rows_people) ---
            row($a, W::AgentTokenMergeApprove, OTHER_MR, deny(E::Forbidden)),
            // --- the scope: only `merge_approve` satisfies `merge_approve` ---
            row($a, W::DevTokenApi, OTHER_MR, deny(E::InsufficientScope)),
            row($a, W::AgentTokenApi, OTHER_MR, deny(E::InsufficientScope)),
            row($a, W::AdminTokenAdmin, OTHER_MR, deny(E::InsufficientScope)),
            row(
                $a,
                W::DevTokenMergeExecute,
                OTHER_MR,
                deny(E::InsufficientScope),
            ),
            row(
                $a,
                W::DevTokenWriteRepository,
                OTHER_MR,
                deny(E::InsufficientScope),
            ),
            // --- roles, visibility, accounts ---
            row($a, W::Reporter, OTHER_MR, deny(E::Forbidden)),
            row($a, W::Guest, OTHER_MR, deny(E::Forbidden)),
            row($a, W::NonMember, mr_on(MP::Internal), deny(E::Forbidden)),
            row($a, W::NonMember, OTHER_MR, deny(E::NotFound)),
            row($a, W::Anonymous, OTHER_MR, deny(E::Unauthenticated)),
            row($a, W::Disabled, OTHER_MR, deny(E::AccountDisabled)),
            row(
                $a,
                W::Developer,
                mr_on(MP::Deleted),
                deny(E::PendingDeletion),
            ),
            row(
                $a,
                W::Developer,
                F::Mr(MF {
                    branches: MB::Same,
                    ..MF::other()
                }),
                deny(E::Forbidden),
            ),
            row(
                $a,
                W::Developer,
                F::Mr(MF {
                    branches: MB::TagTarget,
                    ..MF::other()
                }),
                deny(E::Forbidden),
            ),
        ]
    };
}

const APPROVE: [Row; 25] = approval_rows!(A::MergeRequestApprove);
const UNAPPROVE: [Row; 25] = approval_rows!(A::MergeRequestUnapprove);

#[rustfmt::skip]
const OTHERS: [Row; 18] = [
    // The R4 debt (ADR-0024 §8.5): `merge_approve` never merges.
    row(A::MergeRequestMerge, W::DevTokenMergeApprove, OTHER_MR, deny(E::InsufficientScope)),
    // --- approval rules: people only, Maintainer and above, `api` ---
    row(A::ApprovalRuleManage, W::Maintainer, F::PrivateProject, Allow),
    row(A::ApprovalRuleManage, W::Owner, F::PrivateProject, Allow),
    row(A::ApprovalRuleManage, W::Admin, F::PrivateProject, Allow),
    row(A::ApprovalRuleManage, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::ApprovalRuleManage, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::ApprovalRuleManage, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::ApprovalRuleManage, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::ApprovalRuleManage, W::OwnerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::ApprovalRuleManage, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::ApprovalRuleManage, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::ApprovalRuleManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::ApprovalRuleManage, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::ApprovalRuleManage, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::ApprovalRuleManage, W::DevTokenMergeApprove, F::PrivateProject, deny(E::InsufficientScope)),
    // R5 review: neither merge scope, nor a Maintainer's token without `api`.
    row(A::ApprovalRuleManage, W::DevTokenMergeExecute, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::ApprovalRuleManage, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    // An administrator's `merge_approve` + `admin` token approves another's
    // merge request (whether it counts is evaluate_merge()'s: a non-member
    // administrator's approval does not count).
    row(A::MergeRequestApprove, W::AdminTokenMergeApproveAdmin, OTHER_MR, Allow),
];

const ALL: [[Row; 25]; 2] = [APPROVE, UNAPPROVE];

/// The approve and unapprove rows (`matrix::ROWS` appends them).
pub(crate) const APPROVAL_ROWS: &[Row] = ALL.as_flattened();
/// The approval rule rows and the R4 scope debt.
pub(crate) const APPROVAL_OTHER_ROWS: &[Row] = &OTHERS;
