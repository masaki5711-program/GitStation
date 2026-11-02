//! The service account and bot deny rows of every action kept for people
//! (ADR-0024 §8.4, M3 R3 review): `authorize()`'s `people_only` refuses
//! `users.kind = service_account` / `bot` whatever the role. Each row uses
//! an actor whose human twin (`Actor::human_twin`: same role, scopes and
//! administrator flag) is allowed on the same fixture — checked by
//! `people_only_rows_are_allowed_for_the_human_twin` — so removing the arm
//! (or an action's `people_only()` call) turns these rows red.
//! `DiscussionResolve`'s rows (service account and bot, own and another's
//! thread) are in `matrix_rows_discussion.rs`.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};
use crate::matrix_review::MrFixture as MF;
use crate::model::{MembershipChange as M, Role as R};

const fn deny(action: A, actor: W, fixture: F) -> Row {
    Row {
        action,
        actor,
        fixture,
        expect: Expect::Deny(E::Forbidden),
    }
}

/// A service account row and a bot row for one action.
macro_rules! both {
    ($action:expr, $sa:expr, $bot:expr, $fixture:expr) => {
        [deny($action, $sa, $fixture), deny($action, $bot, $fixture)]
    };
}

const OWNER: (W, W) = (W::ServiceAccountOwnerTokenApi, W::BotOwnerTokenApi);
const MAINTAINER: (W, W) = (
    W::ServiceAccountMaintainerTokenApi,
    W::BotMaintainerTokenApi,
);
const ADMIN_TOKEN: (W, W) = (W::ServiceAccountAdminTokenAdmin, W::BotAdminTokenAdmin);
const ADMIN_SESSION: (W, W) = (W::ServiceAccountAdminSession, W::BotAdminSession);
/// M3 R4: a merge needs `merge_execute`, so the twins hold that scope.
const MERGER: (W, W) = (W::ServiceAccountTokenMergeExecute, W::BotTokenMergeExecute);
/// M3 R5: an approval needs `merge_approve`.
const APPROVER: (W, W) = (W::ServiceAccountTokenMergeApprove, W::BotTokenMergeApprove);

#[rustfmt::skip]
const GROUPS: [[Row; 2]; 18] = [
    both!(A::ProjectVisibilityChange, OWNER.0, OWNER.1, F::MakeInternal),
    both!(A::ProtectedRefManage, MAINTAINER.0, MAINTAINER.1, F::ProtectionChange { from: None, to: Some(R::Maintainer) }),
    both!(A::ProjectTransfer, OWNER.0, OWNER.1, F::Transfer { target: Some(R::Owner) }),
    both!(A::GroupTransfer, OWNER.0, OWNER.1, F::GroupTransfer { target: Some(R::Owner) }),
    both!(A::UserRename, ADMIN_TOKEN.0, ADMIN_TOKEN.1, F::UserRenameOther),
    both!(A::NamespaceTombstoneRelease, OWNER.0, OWNER.1, F::ReleaseMoved { visible: true }),
    both!(A::ProjectDelete, OWNER.0, OWNER.1, F::PrivateProject),
    both!(A::ProjectRestore, OWNER.0, OWNER.1, F::DeletedProject),
    both!(A::MemberManage, MAINTAINER.0, MAINTAINER.1, F::ProjectMember(M::Add(R::Developer))),
    both!(A::UserDisable, ADMIN_TOKEN.0, ADMIN_TOKEN.1, F::Instance),
    both!(A::AdminSessionRevoke, ADMIN_TOKEN.0, ADMIN_TOKEN.1, F::Session { own: false }),
    // Session only: the twin is the administrator's session.
    both!(A::MergeRequestVersionRefsPurge, ADMIN_SESSION.0, ADMIN_SESSION.1, F::Mr(MF::other())),
    both!(A::MergeRequestMerge, MERGER.0, MERGER.1, F::Mr(MF::other())),
    both!(A::MergeRequestApprove, APPROVER.0, APPROVER.1, F::Mr(MF::other())),
    both!(A::MergeRequestUnapprove, APPROVER.0, APPROVER.1, F::Mr(MF::other())),
    both!(A::ApprovalRuleManage, MAINTAINER.0, MAINTAINER.1, F::PrivateProject),
    // M3 R6.
    both!(A::MergeSettingsManage, MAINTAINER.0, MAINTAINER.1, F::PrivateProject),
    both!(A::CodeownersEnforcementToggle, MAINTAINER.0, MAINTAINER.1, F::Codeowners {
        highest: Some(R::Maintainer),
        change: crate::model::CodeownersToggle::Suspend { minutes: Some(60) },
        deleted: false,
    }),
];

/// The rows, flattened (`matrix::ROWS` appends them), and the Owner
/// token's twin row (the other twins have rows of their own).
pub(crate) const PEOPLE_ONLY_ROWS: &[Row] = GROUPS.as_flattened();

/// A person's Owner `api` token deletes the project the service account
/// and bot Owner tokens may not.
pub(crate) const OWNER_TOKEN_ROWS: &[Row] = &[Row {
    action: A::ProjectDelete,
    actor: W::OwnerTokenApi,
    fixture: F::PrivateProject,
    expect: Expect::Allow,
}];
