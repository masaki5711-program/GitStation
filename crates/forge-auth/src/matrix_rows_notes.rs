//! The permission matrix rows of a person's notes (plan 20 N1, ADR-0040
//! §6, `PersonalNotesCreate` on the caller's own personal namespace):
//! creating the notes project is the person's own decision — only in
//! their own namespace (administrators included: never another's), a
//! person, `api`. An agent's token here, service accounts and bots in
//! `matrix_rows_people.rs`, acting for a person and CI credentials in the
//! generated rows (`ActAsRule::Denied`, `CiRule::Denied`), a portal
//! credential in its own generated rows. Creating the project is also
//! `ProjectCreate` on the namespace (the base matrix's rows), decided
//! again by the command.
//!
//! Reading and writing notes are the existing actions on the notes
//! project (`RepoBrowse`, `RepoPush`, the simple editor's commit): a
//! private project with no other member, so another user is the base
//! matrix's private-project `NonMember` (not found).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

const fn row(actor: W, fixture: F, expect: Expect) -> Row {
    Row {
        action: A::PersonalNotesCreate,
        actor,
        fixture,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

/// Every notes row (`matrix::ROWS` appends them).
#[rustfmt::skip]
pub(crate) const NOTES_ROWS: &[Row] = &[
    row(W::NonMember, F::OwnNamespace, Allow),
    row(W::Developer, F::OwnNamespace, Allow),
    row(W::Admin, F::OwnNamespace, Allow),
    row(W::DevTokenApi, F::OwnNamespace, Allow),
    row(W::OwnerTokenApi, F::OwnNamespace, Allow),
    // Another person's namespace: never, administrators included.
    row(W::NonMember, F::ForeignNamespace, deny(E::Forbidden)),
    row(W::Owner, F::ForeignNamespace, deny(E::Forbidden)),
    row(W::Admin, F::ForeignNamespace, deny(E::Forbidden)),
    row(W::AdminTokenAdmin, F::ForeignNamespace, deny(E::Forbidden)),
    row(W::Admin, F::ServiceAccountNamespace, deny(E::Forbidden)),
    // An agent never creates a person's notes.
    row(W::AgentTokenApi, F::OwnNamespace, deny(E::Forbidden)),
    row(W::AgentOwnerTokenApi, F::OwnNamespace, deny(E::Forbidden)),
    row(W::NonMemberTokenReadApi, F::OwnNamespace, deny(E::InsufficientScope)),
    row(W::DevTokenWriteRepository, F::OwnNamespace, deny(E::InsufficientScope)),
    row(W::DevTokenReadRepository, F::OwnNamespace, deny(E::InsufficientScope)),
    row(W::Anonymous, F::OwnNamespace, deny(E::Unauthenticated)),
    row(W::Disabled, F::OwnNamespace, deny(E::AccountDisabled)),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::ROWS;

    /// Plan 20 N1: a service account, an agent's token, acting for a
    /// person and a CI job are all refused the notes, each by a matrix row
    /// that the matrix test runs; the same person's session is allowed.
    #[test]
    fn notes_create_denied_for_service_account_and_act_as() {
        let rows: Vec<&Row> = ROWS
            .iter()
            .filter(|r| r.action == A::PersonalNotesCreate)
            .collect();
        let denied = |pred: &dyn Fn(W) -> bool, what: &str| {
            let hits: Vec<&&Row> = rows.iter().filter(|r| pred(r.actor)).collect();
            assert!(!hits.is_empty(), "no {what} row for PersonalNotesCreate");
            for r in hits {
                assert!(
                    matches!(r.expect, Expect::Deny(_)),
                    "{what} must be denied: {r:?}"
                );
                let principal = r.actor.principal();
                let resource = r.fixture.resource(r.actor);
                let err = crate::authorize(&principal, A::PersonalNotesCreate, &resource)
                    .expect_err("denied");
                assert_eq!(Expect::Deny(err.code()), r.expect, "{what}: {r:?}");
            }
        };
        denied(
            &|a| matches!(a, W::ServiceAccountMaintainerTokenApi),
            "service account",
        );
        denied(&|a| a == W::AgentTokenApi, "agent token");
        denied(&|a| matches!(a, W::ActAs(_)), "act-as");
        denied(&|a| matches!(a, W::Job(_)), "CI job");
        assert!(rows.iter().any(|r| r.actor == W::NonMember
            && matches!(r.fixture, F::OwnNamespace)
            && r.expect == Allow));
    }
}
