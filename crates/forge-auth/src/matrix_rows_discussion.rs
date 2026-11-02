//! The permission matrix rows of merge request discussions (M3 R3,
//! ADR-0024 §12): `MergeRequestComment` (on the merge request) and
//! `DiscussionResolve` (on the discussion). Kept apart from
//! `matrix_rows.rs` so the review MRs add their rows without growing it.
//!
//! - Commenting: Reporter and above **by membership** (an internal
//!   project's non-member reads but does not comment), agents' tokens and
//!   service accounts included (a comment is no merge condition).
//! - Resolving: people only (`agent_token`, `service_account`, `bot`); the
//!   thread's starter (Reporter and above); another's thread only a
//!   Developer and above who is not the merge request's author —
//!   administrators included (the author never clears
//!   `unresolved_discussions` alone).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::matrix_review::{
    DiscussionFixture as DF, MrBranches as MB, MrFixture as MF, MrProject as MP,
};
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

/// Another's merge request on a project of another kind.
const fn mr_on(project: MP) -> F {
    F::Mr(MF {
        project,
        ..MF::other()
    })
}

/// A discussion on a private project: `own_mr` = the caller authored the
/// merge request, `own_thread` = the caller started the thread.
const fn thread(own_mr: bool, own_thread: bool) -> F {
    F::Discussion(DF {
        project: MP::Private,
        own_mr,
        own_thread,
    })
}

/// Another's thread on another's merge request on a project of `project`.
const fn thread_on(project: MP) -> F {
    F::Discussion(DF {
        project,
        own_mr: false,
        own_thread: false,
    })
}

/// The caller's own thread on another's merge request.
const MINE: F = thread(false, true);
/// Another's thread on another's merge request.
const THEIRS: F = thread(false, false);
/// Another's thread on the caller's own merge request.
const THEIRS_ON_MY_MR: F = thread(true, false);
/// The caller's own thread on the caller's own merge request.
const MINE_ON_MY_MR: F = thread(true, true);

#[rustfmt::skip]
pub(crate) const DISCUSSION_ROWS: &[Row] = &[
    // --- MergeRequestComment (Reporter and above by membership; agents and
    // service accounts may comment: a comment is no merge condition) ---
    row(A::MergeRequestComment, W::Reporter, OTHER_MR, Allow),
    row(A::MergeRequestComment, W::Developer, OWN_MR, Allow),
    row(A::MergeRequestComment, W::Reporter, F::Mr(MF { state: S::Merged, ..MF::other() }), Allow),
    row(A::MergeRequestComment, W::Admin, OTHER_MR, Allow),
    row(A::MergeRequestComment, W::DevTokenApi, OTHER_MR, Allow),
    row(A::MergeRequestComment, W::AgentTokenApi, OTHER_MR, Allow),
    row(A::MergeRequestComment, W::ServiceAccountTokenApi, OTHER_MR, Allow),
    // A bot comments within its role too (MergeRequestComment is not kept
    // for people; only resolving is).
    row(A::MergeRequestComment, W::BotTokenApi, OTHER_MR, Allow),
    row(A::MergeRequestComment, W::Guest, OTHER_MR, deny(E::Forbidden)),
    row(A::MergeRequestComment, W::NonMember, mr_on(MP::Internal), deny(E::Forbidden)),
    row(A::MergeRequestComment, W::NonMember, OTHER_MR, deny(E::NotFound)),
    row(A::MergeRequestComment, W::Anonymous, OTHER_MR, deny(E::Unauthenticated)),
    row(A::MergeRequestComment, W::Disabled, OTHER_MR, deny(E::AccountDisabled)),
    row(A::MergeRequestComment, W::AgentTokenReadApi, OTHER_MR, deny(E::InsufficientScope)),
    row(A::MergeRequestComment, W::DevTokenReadRepository, OTHER_MR, deny(E::InsufficientScope)),
    row(A::MergeRequestComment, W::Developer, mr_on(MP::Deleted), deny(E::PendingDeletion)),
    row(A::MergeRequestComment, W::Developer, F::Mr(MF { branches: MB::ReservedSource, ..MF::other() }), deny(E::Forbidden)),
    // --- DiscussionResolve (people only; the starter, or a Developer who is
    // not the merge request's author) ---
    row(A::DiscussionResolve, W::Reporter, MINE, Allow),
    row(A::DiscussionResolve, W::Developer, THEIRS, Allow),
    row(A::DiscussionResolve, W::Maintainer, THEIRS, Allow),
    row(A::DiscussionResolve, W::Developer, MINE_ON_MY_MR, Allow),
    row(A::DiscussionResolve, W::Admin, THEIRS, Allow),
    row(A::DiscussionResolve, W::DevTokenApi, THEIRS, Allow),
    row(A::DiscussionResolve, W::AgentTokenApi, MINE, deny(E::Forbidden)),
    row(A::DiscussionResolve, W::AgentMaintainerTokenApi, THEIRS, deny(E::Forbidden)),
    row(A::DiscussionResolve, W::ServiceAccountTokenApi, MINE, deny(E::Forbidden)),
    row(A::DiscussionResolve, W::ServiceAccountTokenApi, THEIRS, deny(E::Forbidden)),
    row(A::DiscussionResolve, W::BotTokenApi, MINE, deny(E::Forbidden)),
    row(A::DiscussionResolve, W::BotTokenApi, THEIRS, deny(E::Forbidden)),
    row(A::DiscussionResolve, W::Reporter, THEIRS, deny(E::Forbidden)),
    row(A::DiscussionResolve, W::Developer, THEIRS_ON_MY_MR, deny(E::Forbidden)),
    row(A::DiscussionResolve, W::Maintainer, THEIRS_ON_MY_MR, deny(E::Forbidden)),
    row(A::DiscussionResolve, W::Admin, THEIRS_ON_MY_MR, deny(E::Forbidden)),
    row(A::DiscussionResolve, W::Guest, MINE, deny(E::Forbidden)),
    row(A::DiscussionResolve, W::NonMember, F::Discussion(DF { project: MP::Internal, own_mr: false, own_thread: true }), deny(E::Forbidden)),
    row(A::DiscussionResolve, W::NonMember, THEIRS, deny(E::NotFound)),
    row(A::DiscussionResolve, W::Anonymous, THEIRS, deny(E::Unauthenticated)),
    row(A::DiscussionResolve, W::Disabled, MINE, deny(E::AccountDisabled)),
    row(A::DiscussionResolve, W::DevTokenReadRepository, THEIRS, deny(E::InsufficientScope)),
    row(A::DiscussionResolve, W::Developer, thread_on(MP::Deleted), deny(E::PendingDeletion)),
];
