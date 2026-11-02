//! The permission matrix rows of merging a merge request (M3 R4,
//! ADR-0024 §4.3, §8.4, §8.5, §12): `MergeRequestMerge` on the merge
//! request, whose target protection the loader read with the push path's
//! `ProtectionLoader`.
//!
//! - People only: an agent's token (`agent_token`), a service account and
//!   a bot (`service_account` / `bot`, rows in `matrix_rows_people.rs`).
//! - The target's minimum push role, Developer when unprotected;
//!   administrators too. A Maintainer-protected target refuses a Developer.
//! - A token needs `merge_execute`: `api` alone (and `admin`) does not
//!   satisfy it (`FG-AUTH-0002`). The `merge_approve`-only row joins with
//!   that scope (R5).
//! - Readiness (draft, closed, conflicts) is `evaluate_merge()`'s: a draft
//!   or merged merge request is still allowed here.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::matrix_review::{MrBranches as MB, MrFixture as MF, MrProject as MP};
use crate::model::Role as R;
use forge_core::MergeRequestState as S;

const fn row(actor: W, fixture: F, expect: Expect) -> Row {
    Row {
        action: A::MergeRequestMerge,
        actor,
        fixture,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

const OTHER_MR: F = F::Mr(MF::other());

/// Another's merge request whose target is protected at `role`.
const fn protected(role: R) -> F {
    F::Mr(MF {
        protected: Some(role),
        ..MF::other()
    })
}

/// Another's merge request on a project of another kind.
const fn mr_on(project: MP) -> F {
    F::Mr(MF {
        project,
        ..MF::other()
    })
}

#[rustfmt::skip]
pub(crate) const MERGE_ROWS: &[Row] = &[
    // --- allowed: the target's minimum push role (Developer unprotected) ---
    row(W::Developer, OTHER_MR, Allow),
    row(W::Developer, F::Mr(MF::own()), Allow),
    row(W::Maintainer, protected(R::Maintainer), Allow),
    row(W::Developer, protected(R::Developer), Allow),
    row(W::Admin, protected(R::Owner), Allow),
    // Review round 1: an administrator's token with `admin` merges into an
    // Owner-protected target exactly as its push may (administrator rights
    // pass the protection on both paths); without `admin` it is a plain
    // user with no role on the private project.
    row(W::AdminTokenMergeExecuteAdmin, protected(R::Owner), Allow),
    row(W::AdminTokenMergeExecute, protected(R::Owner), deny(E::NotFound)),
    row(W::DevTokenMergeExecute, OTHER_MR, Allow),
    // Readiness is evaluate_merge()'s: a draft or merged one is no denial.
    row(W::Developer, F::Mr(MF { draft: true, ..MF::other() }), Allow),
    row(W::Developer, F::Mr(MF { state: S::Merged, ..MF::other() }), Allow),
    // --- the protection ---
    row(W::Developer, protected(R::Maintainer), deny(E::Forbidden)),
    row(W::Maintainer, protected(R::Owner), deny(E::Forbidden)),
    row(W::DevTokenMergeExecute, protected(R::Maintainer), deny(E::Forbidden)),
    // --- people only (agents here; service accounts and bots in matrix_rows_people) ---
    row(W::AgentTokenMergeExecute, OTHER_MR, deny(E::Forbidden)),
    // --- the scope: `api` (and `admin`) never satisfies `merge_execute` ---
    row(W::DevTokenApi, OTHER_MR, deny(E::InsufficientScope)),
    row(W::AgentTokenApi, OTHER_MR, deny(E::InsufficientScope)),
    row(W::AdminTokenAdmin, OTHER_MR, deny(E::InsufficientScope)),
    row(W::DevTokenWriteRepository, OTHER_MR, deny(E::InsufficientScope)),
    // --- roles, visibility, accounts ---
    row(W::Reporter, OTHER_MR, deny(E::Forbidden)),
    row(W::Guest, OTHER_MR, deny(E::Forbidden)),
    row(W::NonMember, mr_on(MP::Internal), deny(E::Forbidden)),
    row(W::NonMember, OTHER_MR, deny(E::NotFound)),
    row(W::Anonymous, OTHER_MR, deny(E::Unauthenticated)),
    row(W::Disabled, OTHER_MR, deny(E::AccountDisabled)),
    row(W::Developer, mr_on(MP::Deleted), deny(E::PendingDeletion)),
    row(W::Developer, F::Mr(MF { branches: MB::Same, ..MF::other() }), deny(E::Forbidden)),
    row(W::Developer, F::Mr(MF { branches: MB::TagTarget, ..MF::other() }), deny(E::Forbidden)),
];
