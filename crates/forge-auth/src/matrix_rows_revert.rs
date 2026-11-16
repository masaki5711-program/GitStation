//! The permission matrix rows of the one-step revert (M4 C10, plan 14 C10:
//! "既存の MR の Action"). No new action: `POST
//! …/merge-requests/{iid}/revert` is decided as what it does —
//!
//! - `MergeRequestCreate` of the revert merge request: the caller's own,
//!   `refs/heads/revert/mr-<iid>` → the reverted merge request's target
//!   (`MrBranches::RevertToMain`). Developer and above (administrators
//!   too), a token needs `api`; an agent's `api` token may (opening a
//!   merge request is not kept for people). A revert in another person's
//!   name is refused (the route always uses the caller). The system's
//!   draft after a failed pipeline is the same decision for the reverted
//!   merge request's author, with `draft: true`.
//! - `RepoPush` creating the branch `revert/mr-<iid>` on the push path's
//!   protection rules: Developer when unprotected, the protection's
//!   minimum role otherwise (a protected `revert/*` refuses a Developer
//!   with FG-GIT-0008), a token needs `write_repository`.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::matrix_review::{MrBranches as MB, MrFixture as MF, MrProject as MP};
use crate::model::{RefKind, RefUpdate as U, Role as R};

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

/// The caller's revert of merge request !1 (`revert/mr-1` → main).
const REVERT: F = F::Mr(MF {
    branches: MB::RevertToMain,
    ..MF::own()
});

/// The system's draft of it (the author's own, a draft).
const DRAFT: F = F::Mr(MF {
    branches: MB::RevertToMain,
    draft: true,
    ..MF::own()
});

/// A revert opened in another person's name.
const FOR_OTHER: F = F::Mr(MF {
    branches: MB::RevertToMain,
    ..MF::other()
});

const fn on(project: MP) -> F {
    F::Mr(MF {
        branches: MB::RevertToMain,
        project,
        ..MF::own()
    })
}

/// The revert branch's creation (`protected`: `revert/*`'s minimum role).
const fn branch(protected: Option<R>) -> F {
    F::Ref {
        kind: RefKind::Branch,
        update: U::Create,
        protected,
    }
}

#[rustfmt::skip]
pub(crate) const REVERT_ROWS: &[Row] = &[
    // --- who may open the revert merge request ---
    row(A::MergeRequestCreate, W::Developer, REVERT, Allow),
    row(A::MergeRequestCreate, W::Maintainer, REVERT, Allow),
    row(A::MergeRequestCreate, W::Owner, REVERT, Allow),
    row(A::MergeRequestCreate, W::Admin, REVERT, Allow),
    row(A::MergeRequestCreate, W::DevTokenApi, REVERT, Allow),
    row(A::MergeRequestCreate, W::AgentTokenApi, REVERT, Allow),
    row(A::MergeRequestCreate, W::Developer, DRAFT, Allow),
    row(A::MergeRequestCreate, W::Reporter, REVERT, deny(E::Forbidden)),
    row(A::MergeRequestCreate, W::Reporter, DRAFT, deny(E::Forbidden)),
    row(A::MergeRequestCreate, W::Guest, REVERT, deny(E::Forbidden)),
    row(A::MergeRequestCreate, W::NonMember, REVERT, deny(E::NotFound)),
    row(A::MergeRequestCreate, W::NonMember, on(MP::Internal), deny(E::Forbidden)),
    row(A::MergeRequestCreate, W::Anonymous, REVERT, deny(E::Unauthenticated)),
    row(A::MergeRequestCreate, W::OwnerTokenReadApi, REVERT, deny(E::InsufficientScope)),
    row(A::MergeRequestCreate, W::Disabled, REVERT, deny(E::AccountDisabled)),
    row(A::MergeRequestCreate, W::Developer, on(MP::Deleted), deny(E::PendingDeletion)),
    row(A::MergeRequestCreate, W::Developer, FOR_OTHER, deny(E::Forbidden)),
    row(A::MergeRequestCreate, W::Admin, FOR_OTHER, deny(E::Forbidden)),
    // --- who may create the branch revert/mr-<iid> (a push; the base rows
    // of matrix_rows.rs hold Developer allowed, Reporter refused and a
    // Developer refused on a Maintainer-protected branch) ---
    row(A::RepoPush, W::Maintainer, branch(Some(R::Maintainer)), Allow),
    row(A::RepoPush, W::DevTokenApi, branch(None), deny(E::InsufficientScope)),
];
