//! The permission matrix rows of the CLI distribution (`ClientDownload`:
//! `GET /api/v1/clients` and `GET /api/v1/clients/gstn/{target}/binary`).
//! Every active signed-in person downloads `gstn`, a token with `read_api`
//! (an agent's included); anonymous callers, disabled accounts, a token
//! without `read_api`, a runner and a job are refused. Acting for a person
//! is refused by the generated rows (`ActAsRule::Denied`), as are the
//! portal's credential and the CI credentials on a project fixture.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::model::JobTokenState as J;

const fn row(actor: W, expect: Expect) -> Row {
    Row {
        action: A::ClientDownload,
        actor,
        fixture: F::Instance,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

/// Every CLI distribution row (`matrix::ROWS` appends them).
#[rustfmt::skip]
pub(crate) const CLIENT_ROWS: &[Row] = &[
    row(W::Developer, Allow),
    row(W::Admin, Allow),
    row(W::DevTokenApi, Allow),
    row(W::OwnerTokenReadApi, Allow),
    row(W::AgentTokenReadApi, Allow),
    row(W::Anonymous, deny(E::Unauthenticated)),
    row(W::Disabled, deny(E::AccountDisabled)),
    row(W::DisabledAdmin, deny(E::AccountDisabled)),
    row(W::DevTokenReadRepository, deny(E::InsufficientScope)),
    row(W::DevTokenWriteRepository, deny(E::InsufficientScope)),
    // A runner and a job download nothing through the API.
    row(W::Runner, deny(E::Forbidden)),
    row(W::Job(J::Live), deny(E::Forbidden)),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows decide as `authorize()` does, and the CI credentials are
    /// refused on the instance itself (not only on a project fixture).
    #[test]
    fn client_download_rows_hold_and_ci_is_refused() {
        for r in CLIENT_ROWS {
            let principal = r.actor.principal();
            let resource = r.fixture.resource(r.actor);
            let got = match crate::authorize(&principal, r.action, &resource) {
                Ok(()) => Allow,
                Err(e) => Expect::Deny(e.code()),
            };
            assert_eq!(got, r.expect, "{r:?}");
        }
        assert!(CLIENT_ROWS
            .iter()
            .any(|r| r.actor == W::Runner && r.expect == deny(E::Forbidden)));
        assert!(CLIENT_ROWS
            .iter()
            .any(|r| matches!(r.actor, W::Job(_)) && r.expect == deny(E::Forbidden)));
    }
}
