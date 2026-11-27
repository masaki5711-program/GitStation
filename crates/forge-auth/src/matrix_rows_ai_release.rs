//! The permission matrix rows of `pfs-ai-bundle` releases (plan 17 P17-7,
//! ADR-0031 Amendment P17-7).
//!
//! - `AiAssetReleaseCreate` is a person's, Maintainer and above, with an
//!   `api` token too (`gstn ai-asset release`): a release changes no
//!   environment. An agent's token, a bot (`pfs-ai-bot`), a service
//!   account (`matrix_rows_people.rs`), the auto-merge principal, a CI job
//!   and an act-as credential (generated) are refused.
//! - Releases are read with `DeployRead` and deployed or rolled back with
//!   `DeployCreate` (their rows: `matrix_rows_deploy.rs`); every stage is
//!   approved with `DeployApprove` — people only, a web session — and the
//!   prod approval is never a bot's (`prod_approve_denied_for_bot`).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

const fn row(actor: W, fixture: F, expect: Expect) -> Row {
    Row {
        action: A::AiAssetReleaseCreate,
        actor,
        fixture,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

#[rustfmt::skip]
pub(crate) const AI_RELEASE_ROWS: &[Row] = &[
    row(W::Maintainer, F::PrivateProject, Allow),
    row(W::Owner, F::PrivateProject, Allow),
    row(W::Admin, F::PrivateProject, Allow),
    // A person's token with `api` (gstn).
    row(W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(W::OwnerTokenApi, F::PrivateProject, Allow),
    row(W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(W::DevTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(W::Guest, F::PrivateProject, deny(E::Forbidden)),
    // An agent's token, whatever its role (people only).
    row(W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(W::AgentOwnerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    // A bot — pfs-ai-bot is one — even an administrator's session.
    row(W::BotAdminSession, F::PrivateProject, deny(E::Forbidden)),
    row(W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix_rows_deploy::PROD;

    fn reason(e: &forge_errors::ForgeError) -> String {
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn prod_approve_denied_for_bot() {
        // pfs-ai-prod is the prod tier: its approval is a person's in a
        // session. A bot is refused on its kind in a session and before
        // the role with a token.
        let bot = crate::authorize(
            &W::BotAdminSession.principal(),
            A::DeployApprove,
            &PROD.resource(W::BotAdminSession),
        )
        .expect_err("a bot never approves prod");
        assert_eq!(reason(&bot), "bot");
        for actor in [W::BotMaintainerTokenApi, W::BotOwnerTokenApi] {
            let e = crate::authorize(&actor.principal(), A::DeployApprove, &PROD.resource(actor))
                .expect_err("a bot's token never approves prod");
            assert_eq!(reason(&e), "session_required", "{actor:?}");
        }
        // Nor an agent, a service account or the auto-merge principal.
        for actor in [
            W::AgentMaintainerTokenApi,
            W::ServiceAccountAdminSession,
            W::AutoMerge,
        ] {
            assert!(
                crate::authorize(&actor.principal(), A::DeployApprove, &PROD.resource(actor))
                    .is_err(),
                "{actor:?}"
            );
        }
        // A Maintainer's session approves it.
        crate::authorize(
            &W::Maintainer.principal(),
            A::DeployApprove,
            &PROD.resource(W::Maintainer),
        )
        .expect("a person approves");
    }

    #[test]
    fn release_denied_for_bot_agent_and_service_account() {
        for actor in [
            W::BotAdminSession,
            W::BotMaintainerTokenApi,
            W::AgentMaintainerTokenApi,
            W::ServiceAccountAdminSession,
            W::ServiceAccountMaintainerTokenApi,
            W::AutoMerge,
        ] {
            assert!(
                crate::authorize(
                    &actor.principal(),
                    A::AiAssetReleaseCreate,
                    &F::PrivateProject.resource(actor)
                )
                .is_err(),
                "{actor:?}"
            );
        }
    }
}
