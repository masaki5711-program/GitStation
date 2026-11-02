//! The permission matrix rows of the merge conditions (M3 R6, ADR-0024
//! §4.3, §9, §12).
//!
//! - `MergeSettingsManage` on the project: people only (an agent's token
//!   here; service accounts and bots in `matrix_rows_people.rs`),
//!   Maintainer and above, `api`.
//! - `CodeownersEnforcementToggle` on `Resource::CodeownersEnforcement`:
//!   people only, Maintainer and above; a suspension also needs a role at
//!   or above every protection of the project (`above_own_role`) and an
//!   expiry of 1 to 1440 minutes (`expiry_missing`,
//!   `expiry_out_of_range`); enforcing again is any Maintainer's.
//! - The direct-push ban (`RepoPush` on a branch whose merge rule forbids
//!   direct pushes): `FG-GIT-0017` for every caller, administrators
//!   included; force and delete keep their own codes; a merge into such a
//!   branch is allowed (`MergeRequestMerge` never reads the ban).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::matrix_review::MrFixture as MF;
use crate::model::{CodeownersToggle as T, RefUpdate as U, Role as R};

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

/// A suspension for `minutes` on a project whose highest protection is
/// `highest`.
const fn suspend(highest: Option<R>, minutes: Option<u32>) -> F {
    F::Codeowners {
        highest,
        change: T::Suspend { minutes },
        deleted: false,
    }
}

const HOUR: F = suspend(Some(R::Maintainer), Some(60));
const ENFORCE_OWNER_LEVEL: F = F::Codeowners {
    highest: Some(R::Owner),
    change: T::Enforce,
    deleted: false,
};
const NO_DIRECT_PUSH_MR: F = F::Mr(MF {
    protected: Some(R::Maintainer),
    no_direct_push: true,
    ..MF::other()
});

#[rustfmt::skip]
pub(crate) const CONDITION_ROWS: &[Row] = &[
    // --- merge settings: people only, Maintainer and above, `api` ---
    row(A::MergeSettingsManage, W::Maintainer, F::PrivateProject, Allow),
    row(A::MergeSettingsManage, W::Owner, F::PrivateProject, Allow),
    row(A::MergeSettingsManage, W::Admin, F::PrivateProject, Allow),
    row(A::MergeSettingsManage, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::MergeSettingsManage, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeSettingsManage, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeSettingsManage, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeSettingsManage, W::OwnerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::MergeSettingsManage, W::DevTokenMergeExecute, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::MergeSettingsManage, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::MergeSettingsManage, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::MergeSettingsManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::MergeSettingsManage, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::MergeSettingsManage, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
    // --- CODEOWNERS enforcement: allowed ---
    row(A::CodeownersEnforcementToggle, W::Maintainer, HOUR, Allow),
    row(A::CodeownersEnforcementToggle, W::Maintainer, suspend(None, Some(1)), Allow),
    row(A::CodeownersEnforcementToggle, W::Maintainer, suspend(Some(R::Developer), Some(1440)), Allow),
    row(A::CodeownersEnforcementToggle, W::Owner, suspend(Some(R::Owner), Some(60)), Allow),
    row(A::CodeownersEnforcementToggle, W::Admin, suspend(Some(R::Owner), Some(60)), Allow),
    row(A::CodeownersEnforcementToggle, W::MaintainerTokenApi, HOUR, Allow),
    // Enforcing again only tightens: any Maintainer, whatever the protections.
    row(A::CodeownersEnforcementToggle, W::Maintainer, ENFORCE_OWNER_LEVEL, Allow),
    // --- above one's own role, and the expiry ---
    row(A::CodeownersEnforcementToggle, W::Maintainer, suspend(Some(R::Owner), Some(60)), deny(E::Forbidden)),
    row(A::CodeownersEnforcementToggle, W::Maintainer, suspend(Some(R::Maintainer), None), deny(E::Forbidden)),
    row(A::CodeownersEnforcementToggle, W::Maintainer, suspend(Some(R::Maintainer), Some(1441)), deny(E::Forbidden)),
    row(A::CodeownersEnforcementToggle, W::Maintainer, suspend(Some(R::Maintainer), Some(0)), deny(E::Forbidden)),
    row(A::CodeownersEnforcementToggle, W::Owner, suspend(None, None), deny(E::Forbidden)),
    // --- people only, roles, scope, visibility, accounts ---
    row(A::CodeownersEnforcementToggle, W::AgentMaintainerTokenApi, HOUR, deny(E::Forbidden)),
    row(A::CodeownersEnforcementToggle, W::Developer, HOUR, deny(E::Forbidden)),
    row(A::CodeownersEnforcementToggle, W::Developer, ENFORCE_OWNER_LEVEL, deny(E::Forbidden)),
    row(A::CodeownersEnforcementToggle, W::OwnerTokenReadApi, HOUR, deny(E::InsufficientScope)),
    row(A::CodeownersEnforcementToggle, W::DevTokenMergeApprove, HOUR, deny(E::InsufficientScope)),
    row(A::CodeownersEnforcementToggle, W::NonMember, HOUR, deny(E::NotFound)),
    row(A::CodeownersEnforcementToggle, W::Anonymous, HOUR, deny(E::Unauthenticated)),
    row(A::CodeownersEnforcementToggle, W::Disabled, HOUR, deny(E::AccountDisabled)),
    row(
        A::CodeownersEnforcementToggle,
        W::Maintainer,
        F::Codeowners { highest: None, change: T::Suspend { minutes: Some(60) }, deleted: true },
        deny(E::PendingDeletion),
    ),
    // --- the direct-push ban (RepoPush): everyone, administrators included ---
    row(A::RepoPush, W::Maintainer, F::RefNoDirectPush { update: U::FastForward }, deny(E::ProtectedRefDirectPushForbidden)),
    row(A::RepoPush, W::Owner, F::RefNoDirectPush { update: U::Create }, deny(E::ProtectedRefDirectPushForbidden)),
    row(A::RepoPush, W::Admin, F::RefNoDirectPush { update: U::FastForward }, deny(E::ProtectedRefDirectPushForbidden)),
    row(A::RepoPush, W::Owner, F::RefNoDirectPush { update: U::Force }, deny(E::ProtectedRefForcePush)),
    row(A::RepoPush, W::Owner, F::RefNoDirectPush { update: U::Delete }, deny(E::ProtectedRefDeletion)),
    // The same branch is merged into: the ban is not the merge's to read.
    row(A::MergeRequestMerge, W::Maintainer, NO_DIRECT_PUSH_MR, Allow),
    row(A::MergeRequestMerge, W::Developer, NO_DIRECT_PUSH_MR, deny(E::Forbidden)),
];
