//! The permission matrix rows of the project wiki link (plan 20 W1,
//! ADR-0040 §5, `ProjectWikiLink` on the owner project): creating, linking
//! and unlinking a wiki is a person's decision — Maintainer of the owner
//! project and above, `api`; an agent's token here, service accounts and
//! bots in `matrix_rows_people.rs`, acting for a person and CI credentials
//! in the generated rows (`ActAsRule::Denied`, `CiRule::Denied`), a portal
//! credential in its own generated rows. Creating the wiki project is also
//! `ProjectCreate` on the owner's namespace (its rows are the base
//! matrix's), decided again by the command.
//!
//! The wiki's own reads and pushes are the existing actions on the wiki
//! project: the inherited role is computed by `forge_db::roles` (one place)
//! and arrives in `ProjectView::role`, so their rows are the base matrix's;
//! forge-db's `wikis` tests prove the inheritance itself.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

const fn row(actor: W, fixture: F, expect: Expect) -> Row {
    Row {
        action: A::ProjectWikiLink,
        actor,
        fixture,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

/// Every wiki link row (`matrix::ROWS` appends them).
#[rustfmt::skip]
pub(crate) const WIKI_LINK_ROWS: &[Row] = &[
    row(W::Maintainer, F::PrivateProject, Allow),
    row(W::Owner, F::PrivateProject, Allow),
    row(W::Admin, F::PrivateProject, Allow),
    row(W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(W::Guest, F::PrivateProject, deny(E::Forbidden)),
    // An agent never links a wiki (its members would gain access).
    row(W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(W::OwnerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::ROWS;

    /// Plan 20 W1: an agent's token, a service account, acting for a
    /// person and a CI job are all refused the link, each by a matrix row
    /// that the matrix test runs (`authorize()` on the row's principal and
    /// fixture).
    #[test]
    fn link_denied_for_agent_service_account_act_as() {
        let rows: Vec<&Row> = ROWS
            .iter()
            .filter(|r| r.action == A::ProjectWikiLink)
            .collect();
        let denied = |pred: &dyn Fn(W) -> bool, what: &str| {
            let hits: Vec<&&Row> = rows.iter().filter(|r| pred(r.actor)).collect();
            assert!(!hits.is_empty(), "no {what} row for ProjectWikiLink");
            for r in hits {
                assert!(
                    matches!(r.expect, Expect::Deny(_)),
                    "{what} must be denied: {r:?}"
                );
                let principal = r.actor.principal();
                let resource = r.fixture.resource(r.actor);
                let err = crate::authorize(&principal, A::ProjectWikiLink, &resource)
                    .expect_err("denied");
                assert_eq!(Expect::Deny(err.code()), r.expect, "{what}: {r:?}");
            }
        };
        denied(&|a| a == W::AgentMaintainerTokenApi, "agent token");
        denied(
            &|a| a == W::ServiceAccountMaintainerTokenApi,
            "service account",
        );
        denied(&|a| matches!(a, W::ActAs(_)), "act-as");
        denied(&|a| matches!(a, W::Job(_)), "CI job");
        // The same person's session is allowed (the twin).
        assert!(rows
            .iter()
            .any(|r| r.actor == W::Maintainer && r.expect == Allow));
    }
}
