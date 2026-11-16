//! The permission matrix rows of `ai_assets.enabled` (plan 17 §0.4, P17-3,
//! ADR-0035): `AiAssetsSettingManage` is a person's, Maintainer and above,
//! in a web session only.
//!
//! - A token is refused whatever its role and scopes (`session_required`):
//!   a person's, an agent's (`is_agent`).
//! - A service account and a bot (`pfs-ai-bot` is one) are refused even in
//!   a session (`people_only`): `matrix_rows_people.rs`.
//! - The auto-merge principal is refused every action but its two
//!   (`matrix_rows_auto_merge::generated_denies`); a CI job and an act-as
//!   credential likewise (generated from `ci_rule` / `act_as_rule`).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};

const fn row(actor: W, fixture: F, expect: Expect) -> Row {
    Row {
        action: A::AiAssetsSettingManage,
        actor,
        fixture,
        expect,
    }
}

const fn deny(code: E) -> Expect {
    Expect::Deny(code)
}

#[rustfmt::skip]
pub(crate) const AI_ASSETS_ROWS: &[Row] = &[
    row(W::Maintainer, F::PrivateProject, Allow),
    row(W::Owner, F::PrivateProject, Allow),
    row(W::Admin, F::PrivateProject, Allow),
    row(W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(W::Guest, F::PrivateProject, deny(E::Forbidden)),
    // ai_assets_setting_denied_for_token_session: a token, a person's too.
    row(W::MaintainerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(W::OwnerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(W::AdminTokenAdmin, F::PrivateProject, deny(E::InsufficientScope)),
    // An agent's token, whatever its role.
    row(W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(W::AgentOwnerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(actor: W) -> Result<(), forge_errors::ForgeError> {
        crate::authorize(
            &actor.principal(),
            A::AiAssetsSettingManage,
            &F::PrivateProject.resource(actor),
        )
    }

    fn reason(actor: W) -> String {
        let e = decide(actor).expect_err("denied");
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn ai_assets_setting_denied_for_bot() {
        // A bot (pfs-ai-bot is one), even an administrator's session.
        for bot in [
            W::BotAdminSession,
            W::BotMaintainerTokenApi,
            W::BotOwnerTokenApi,
        ] {
            assert!(decide(bot).is_err(), "{bot:?}");
        }
        assert_eq!(reason(W::BotAdminSession), "bot");
        assert!(decide(W::Admin).is_ok());
    }

    #[test]
    fn ai_assets_setting_denied_for_agent_service_account_and_auto_merge() {
        for actor in [
            W::AgentMaintainerTokenApi,
            W::AgentOwnerTokenApi,
            W::AgentAdminTokenAdmin,
            W::ServiceAccountAdminSession,
            W::ServiceAccountMaintainerTokenApi,
            W::AutoMerge,
        ] {
            assert!(decide(actor).is_err(), "{actor:?}");
        }
        assert_eq!(reason(W::ServiceAccountAdminSession), "service_account");
        assert_eq!(reason(W::AutoMerge), "auto_merge_not_allowed");
    }

    #[test]
    fn ai_assets_setting_denied_for_token_session() {
        for actor in [W::MaintainerTokenApi, W::OwnerTokenApi, W::AdminTokenAdmin] {
            assert_eq!(reason(actor), "session_required", "{actor:?}");
        }
        assert!(decide(W::Maintainer).is_ok());
        assert!(decide(W::Developer).is_err());
    }
}
