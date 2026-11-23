//! The permission matrix rows of org bindings (ADR-0038 §2, §5):
//!
//! - `OrgBindingManage` (add or remove a binding): the group's or
//!   project's Owner, administrators pass; a person in a web session (a
//!   token is `FG-AUTH-0002 reason=session_required`, before anything is
//!   looked at; a service account or bot is `FG-AUTH-0003`, rows in
//!   `matrix_rows_people.rs`). Reading the bindings is `MemberList`.
//! - A binding gives at most Developer (forge-db: the command's
//!   FG-AUTH-0031, the table's CHECK and `forge.org_binding_role`), and the
//!   role loaders hand `authorize()` that role like any other. The
//!   Developer rows of `matrix_rows.rs` and the test below pin what a person
//!   brought in by a binding can and cannot do — push, but never the
//!   Maintainer's operations (protection, members, settings), approve a
//!   deployment or change the bindings themselves. Reading the bindings is
//!   `MemberList` (its rows in `matrix_rows.rs`).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

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

const M: A = A::OrgBindingManage;

#[rustfmt::skip]
pub(crate) const ORG_BINDING_ROWS: &[Row] = &[
    // --- OrgBindingManage on a project: the Owner, a person, a session ---
    row(M, W::Owner, F::PrivateProject, Allow),
    row(M, W::Owner, F::InternalProject, Allow),
    row(M, W::Admin, F::PrivateProject, Allow),
    row(M, W::Maintainer, F::PrivateProject, deny(E::Forbidden)),
    row(M, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(M, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(M, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(M, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(M, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(M, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(M, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(M, W::Owner, F::DeletedProject, deny(E::PendingDeletion)),
    row(M, W::OwnerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(M, W::AdminTokenAdmin, F::PrivateProject, deny(E::InsufficientScope)),
    row(M, W::AgentOwnerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    // --- OrgBindingManage on a group ---
    row(M, W::Owner, F::Group, Allow),
    row(M, W::Owner, F::PrivateGroup, Allow),
    row(M, W::Admin, F::Group, Allow),
    row(M, W::Admin, F::PrivateGroup, Allow),
    row(M, W::Maintainer, F::Group, deny(E::Forbidden)),
    row(M, W::Developer, F::Group, deny(E::Forbidden)),
    row(M, W::Reporter, F::Group, deny(E::Forbidden)),
    row(M, W::Guest, F::Group, deny(E::Forbidden)),
    row(M, W::Maintainer, F::PrivateGroup, deny(E::Forbidden)),
    row(M, W::Developer, F::PrivateGroup, deny(E::Forbidden)),
    row(M, W::Reporter, F::PrivateGroup, deny(E::Forbidden)),
    row(M, W::Guest, F::PrivateGroup, deny(E::Forbidden)),
    row(M, W::NonMember, F::Group, deny(E::Forbidden)),
    row(M, W::NonMember, F::PrivateGroup, deny(E::NotFound)),
    row(M, W::NonMember, F::PrivateGroupPassThrough, deny(E::NotFound)),
    row(M, W::Anonymous, F::Group, deny(E::Unauthenticated)),
    row(M, W::Disabled, F::Group, deny(E::AccountDisabled)),
    row(M, W::OwnerTokenApi, F::Group, deny(E::InsufficientScope)),
    row(M, W::NonMemberTokenReadApi, F::PrivateGroupPassThrough, deny(E::InsufficientScope)),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MembershipChange, Role};

    fn reason(actor: W, fixture: F) -> String {
        let e =
            crate::authorize(&actor.principal(), M, &fixture.resource(actor)).expect_err("denied");
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn org_binding_manage_is_a_persons_in_a_session() {
        for actor in [W::OwnerTokenApi, W::AdminTokenAdmin, W::AgentOwnerTokenApi] {
            assert_eq!(
                reason(actor, F::PrivateProject),
                "session_required",
                "{actor:?}"
            );
        }
        assert_eq!(
            reason(W::ServiceAccountAdminSession, F::Group),
            "service_account"
        );
        assert_eq!(reason(W::BotAdminSession, F::Group), "bot");
        assert_eq!(
            reason(W::AutoMerge, F::PrivateProject),
            "auto_merge_not_allowed"
        );
    }

    /// The most a binding gives (Developer): a push, never a Maintainer's
    /// or an Owner's operation, never a deployment approval.
    #[test]
    fn the_developer_a_binding_gives_cannot_reach_maintainer_operations() {
        let allowed = |action: A, fixture: F| {
            crate::authorize(
                &W::Developer.principal(),
                action,
                &fixture.resource(W::Developer),
            )
            .is_ok()
        };
        assert!(allowed(A::RepoPush, F::PrivateProject));
        for (action, fixture) in [
            (M, F::PrivateProject),
            (M, F::Group),
            (A::ProjectUpdate, F::PrivateProject),
            (A::AiAssetsSettingManage, F::PrivateProject),
            (
                A::MemberManage,
                F::ProjectMember(MembershipChange::Add(Role::Guest)),
            ),
            (
                A::ProtectedRefManage,
                F::ProtectionChange {
                    from: None,
                    to: Some(Role::Developer),
                },
            ),
            (A::DeployApprove, crate::matrix_rows_deploy::STG),
        ] {
            assert!(!allowed(action, fixture), "{action:?}");
        }
    }
}
