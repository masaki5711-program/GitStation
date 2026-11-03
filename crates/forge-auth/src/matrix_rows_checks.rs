//! The permission matrix rows of the Checks API (M3 C1, ADR-0024 §10,
//! §12; plan 11 §5 C1): who reports a check run in which namespace, who
//! reads them, and who grants a namespace to a service account.
//!
//! - `CheckWrite` (`api`): `ci/` and `kanflow/` only by a service account
//!   holding a grant of the namespace, a Developer (or above) member of the
//!   project. People — administrators included — are refused
//!   (`not_service_account`); `forge/` is forge's own worker and never
//!   written over HTTP (`forge_namespace_internal`, administrators too);
//!   `ext/` is refused for everyone in C1 (`ext_namespace_unavailable`);
//!   an agent's token is refused (`agent_token`); a `read_api` token lacks
//!   the scope; a project pending deletion is refused; a project the caller
//!   cannot see is the same 404 as a missing one.
//! - `CheckRead` (`read_api`): as `RepoBrowse`.
//! - `CheckNamespaceGrantManage` (`api`): administrators, people only; a
//!   target of another kind is not found.

use forge_core::CheckNamespace as Ns;
use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};
use crate::matrix_review::MrProject as P;

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

const fn run(namespace: Ns, granted: bool, project: P) -> F {
    F::CheckRun {
        namespace,
        granted,
        project,
    }
}

/// A `ci/` report on a private project, the reporter holding the grant.
const CI: F = run(Ns::Ci, true, P::Private);
const CI_UNGRANTED: F = run(Ns::Ci, false, P::Private);
const KANFLOW: F = run(Ns::Kanflow, true, P::Private);
/// `granted` is a grant of the name's own namespace (`false`: the reporter
/// holds another namespace or none).
const CI_INTERNAL: F = run(Ns::Ci, true, P::Internal);
const CI_DELETED: F = run(Ns::Ci, true, P::Deleted);
const FORGE: F = run(Ns::Forge, false, P::Private);
const EXT: F = run(Ns::Ext, false, P::Private);

const SA: F = F::ServiceAccountTarget {
    human: false,
    active: true,
};
const SA_DISABLED: F = F::ServiceAccountTarget {
    human: false,
    active: false,
};
const PERSON: F = F::ServiceAccountTarget {
    human: true,
    active: true,
};

#[rustfmt::skip]
pub(crate) const CHECK_ROWS: &[Row] = &[
    // CheckWrite: a granted service account, Developer and above.
    row(A::CheckWrite, W::ServiceAccountTokenApi, CI, Allow),
    row(A::CheckWrite, W::ServiceAccountTokenApi, KANFLOW, Allow),
    row(A::CheckWrite, W::ServiceAccountMaintainerTokenApi, CI, Allow),
    row(A::CheckWrite, W::ServiceAccountOwnerTokenApi, CI, Allow),
    // A grant of the namespace is required.
    row(A::CheckWrite, W::ServiceAccountTokenApi, CI_UNGRANTED, deny(E::Forbidden)),
    // A Reporter member is no reporter.
    row(A::CheckWrite, W::ServiceAccountReporterTokenApi, CI, deny(E::Forbidden)),
    // People are refused for ci/ and kanflow/, administrators included.
    row(A::CheckWrite, W::Developer, CI, deny(E::Forbidden)),
    row(A::CheckWrite, W::DevTokenApi, CI, deny(E::Forbidden)),
    row(A::CheckWrite, W::OwnerTokenApi, KANFLOW, deny(E::Forbidden)),
    row(A::CheckWrite, W::Admin, CI, deny(E::Forbidden)),
    row(A::CheckWrite, W::AdminTokenAdmin, CI, deny(E::Forbidden)),
    row(A::CheckWrite, W::BotTokenApi, CI, deny(E::Forbidden)),
    // forge/ is never written over HTTP, by anyone.
    row(A::CheckWrite, W::ServiceAccountTokenApi, FORGE, deny(E::Forbidden)),
    row(A::CheckWrite, W::Admin, FORGE, deny(E::Forbidden)),
    row(A::CheckWrite, W::AdminTokenAdmin, FORGE, deny(E::Forbidden)),
    row(A::CheckWrite, W::ServiceAccountAdminTokenAdmin, FORGE, deny(E::Forbidden)),
    // ext/ is refused for everyone in C1.
    row(A::CheckWrite, W::DevTokenApi, EXT, deny(E::Forbidden)),
    row(A::CheckWrite, W::MaintainerTokenApi, EXT, deny(E::Forbidden)),
    row(A::CheckWrite, W::Admin, EXT, deny(E::Forbidden)),
    row(A::CheckWrite, W::ServiceAccountTokenApi, EXT, deny(E::Forbidden)),
    // Agents' tokens never report (a person's agent must not turn its own
    // merge request green).
    row(A::CheckWrite, W::AgentTokenApi, CI, deny(E::Forbidden)),
    row(A::CheckWrite, W::AgentTokenApi, EXT, deny(E::Forbidden)),
    row(A::CheckWrite, W::AgentOwnerTokenApi, KANFLOW, deny(E::Forbidden)),
    // A read_api token lacks the scope.
    row(A::CheckWrite, W::ServiceAccountTokenReadApi, CI, deny(E::InsufficientScope)),
    row(A::CheckWrite, W::ServiceAccountGuestTokenRead, CI, deny(E::InsufficientScope)),
    // A project pending deletion takes no report.
    row(A::CheckWrite, W::ServiceAccountTokenApi, CI_DELETED, deny(E::PendingDeletion)),
    // A project the service account is no member of is not found (an
    // internal one too: a service account sees member projects only).
    row(A::CheckWrite, W::ServiceAccountNonMemberTokenReadApi, CI, deny(E::NotFound)),
    row(A::CheckWrite, W::ServiceAccountNonMemberTokenReadApi, CI_INTERNAL, deny(E::NotFound)),
    row(A::CheckWrite, W::NonMember, CI, deny(E::NotFound)),
    row(A::CheckWrite, W::Anonymous, CI, deny(E::Unauthenticated)),
    row(A::CheckWrite, W::Disabled, CI, deny(E::AccountDisabled)),
    // CheckRead: as RepoBrowse.
    row(A::CheckRead, W::Reporter, F::PrivateProject, Allow),
    row(A::CheckRead, W::DevTokenApi, F::PrivateProject, Allow),
    row(A::CheckRead, W::OwnerTokenReadApi, F::PrivateProject, Allow),
    row(A::CheckRead, W::NonMember, F::InternalProject, Allow),
    row(A::CheckRead, W::ServiceAccountTokenReadApi, F::PrivateProject, Allow),
    row(A::CheckRead, W::AgentTokenReadApi, F::PrivateProject, Allow),
    row(A::CheckRead, W::Admin, F::PrivateProject, Allow),
    row(A::CheckRead, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(A::CheckRead, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::CheckRead, W::ServiceAccountNonMemberTokenReadApi, F::InternalProject, deny(E::NotFound)),
    row(A::CheckRead, W::DevTokenWriteRepository, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::CheckRead, W::Reporter, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::CheckRead, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    // CheckNamespaceGrantManage: administrators, people only.
    row(A::CheckNamespaceGrantManage, W::Admin, SA, Allow),
    row(A::CheckNamespaceGrantManage, W::AdminTokenAdmin, SA, Allow),
    row(A::CheckNamespaceGrantManage, W::Admin, SA_DISABLED, Allow),
    row(A::CheckNamespaceGrantManage, W::Admin, PERSON, deny(E::NotFound)),
    row(A::CheckNamespaceGrantManage, W::AdminTokenApi, SA, deny(E::Forbidden)),
    row(A::CheckNamespaceGrantManage, W::AgentAdminTokenAdmin, SA, deny(E::Forbidden)),
    row(A::CheckNamespaceGrantManage, W::ServiceAccountAdminTokenAdmin, SA, deny(E::Forbidden)),
    row(A::CheckNamespaceGrantManage, W::BotAdminTokenAdmin, SA, deny(E::Forbidden)),
    row(A::CheckNamespaceGrantManage, W::ServiceAccountAdminSession, SA, deny(E::Forbidden)),
    row(A::CheckNamespaceGrantManage, W::BotAdminSession, SA, deny(E::Forbidden)),
    row(A::CheckNamespaceGrantManage, W::Developer, SA, deny(E::Forbidden)),
    row(A::CheckNamespaceGrantManage, W::Owner, SA, deny(E::Forbidden)),
    row(A::CheckNamespaceGrantManage, W::DisabledAdmin, SA, deny(E::AccountDisabled)),
    row(A::CheckNamespaceGrantManage, W::Anonymous, SA, deny(E::Unauthenticated)),
];

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

    use super::*;
    use crate::authorize;

    fn reason_of(actor: W, fixture: F) -> String {
        let err = authorize(&actor.principal(), A::CheckWrite, &fixture.resource(actor))
            .expect_err("a deny row");
        err.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    /// Each refusal names why in the log (`reason`), so a reporter's
    /// failure is diagnosed from the log line alone (runbook
    /// checks-reporting).
    #[test]
    fn check_write_denials_carry_their_reason() {
        let cases = [
            (W::Admin, CI, "not_service_account"),
            (W::DevTokenApi, KANFLOW, "not_service_account"),
            (W::BotTokenApi, CI, "not_service_account"),
            (
                W::ServiceAccountTokenApi,
                CI_UNGRANTED,
                "namespace_not_granted",
            ),
            (W::Admin, FORGE, "forge_namespace_internal"),
            (W::ServiceAccountTokenApi, FORGE, "forge_namespace_internal"),
            (W::DevTokenApi, EXT, "ext_namespace_unavailable"),
            (W::AgentTokenApi, CI, "agent_token"),
            (W::AgentTokenApi, EXT, "agent_token"),
        ];
        for (actor, fixture, reason) in cases {
            assert_eq!(reason_of(actor, fixture), reason, "{actor:?} {fixture:?}");
        }
    }
}
