//! The permission matrix rows of the local LLM (M5 L5-4, plan 19 §8.2,
//! ADR-0039, ADR-0025 Amendment L5-4). Reading an advice is its subject's
//! read (no rows here); what the reader is shown is
//! `llm::authorize_advice`'s (the tests below).
//!
//! - `AiAdviceFeedback`: whoever reads the advice's subject (a merge
//!   request, a project; an incident diagnosis on the instance: an
//!   administrator, anyone else 404), a person — an agent's token, a
//!   service account and a bot are refused (`people_only`).
//! - `AiReviewRerun`: a Developer, a person.
//! - `LlmSettingsManage` (`llm.review`): a Maintainer, a person, a web
//!   session only (as `AiAssetsSettingManage`).
//! - `OpsDiagnosisRead`: an administrator (a token: the `admin` scope),
//!   an administrator's agent token included; anyone else 404.
//! - `LlmPause` / `LlmResume`: an administrator, a person.
//!
//! The service account and bot rows of the people-only actions are in
//! `matrix_rows_people.rs`; the act-as, CI and auto-merge denies are
//! generated from `act_as_rule` / `ci_rule` / `auto_merge_may`; the act-as
//! reads `PipelineRead` / `DeployRead` are in `matrix_rows_act_as.rs`.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::matrix_review::MrFixture as MF;

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

const MR: F = F::Mr(MF::other());
/// Another's merge request on an internal project.
const INTERNAL_MR: F = F::Mr(MF {
    project: crate::matrix_review::MrProject::Internal,
    ..MF::other()
});

#[rustfmt::skip]
pub(crate) const LLM_ROWS: &[Row] = &[
    // --- AiAdviceFeedback ---
    row(A::AiAdviceFeedback, W::Reporter, F::PrivateProject, Allow),
    row(A::AiAdviceFeedback, W::Reporter, F::InternalProject, Allow),
    row(A::AiAdviceFeedback, W::NonMember, F::InternalProject, Allow),
    row(A::AiAdviceFeedback, W::Developer, MR, Allow),
    row(A::AiAdviceFeedback, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::AiAdviceFeedback, W::DevTokenApi, MR, Allow),
    row(A::AiAdviceFeedback, W::Admin, F::Instance, Allow),
    row(A::AiAdviceFeedback, W::AdminTokenAdmin, F::Instance, Allow),
    // feedback_denied_for_agent_token: an agent's vote would skew the
    // measure of the advice, whatever its role.
    row(A::AiAdviceFeedback, W::AgentTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::AiAdviceFeedback, W::AgentTokenApi, MR, deny(E::Forbidden)),
    row(A::AiAdviceFeedback, W::AgentOwnerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::AiAdviceFeedback, W::AgentAdminTokenAdmin, F::Instance, deny(E::Forbidden)),
    row(A::AiAdviceFeedback, W::ServiceAccountTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::AiAdviceFeedback, W::BotTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::AiAdviceFeedback, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    // M5 L5-6 (the merge request route): the same refusals on a merge request.
    row(A::AiAdviceFeedback, W::Guest, MR, deny(E::Forbidden)),
    row(A::AiAdviceFeedback, W::ServiceAccountTokenApi, MR, deny(E::Forbidden)),
    row(A::AiAdviceFeedback, W::BotTokenApi, MR, deny(E::Forbidden)),
    row(A::AiAdviceFeedback, W::MaintainerTokenReadApi, MR, deny(E::InsufficientScope)),
    row(A::AiAdviceFeedback, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::AiAdviceFeedback, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::AiAdviceFeedback, W::NonMember, MR, deny(E::NotFound)),
    // An agent that cannot see the subject: 404 like anyone (visibility
    // comes before the people-only refusal).
    row(A::AiAdviceFeedback, W::BotNonMemberTokenReadApi, F::PrivateProject, deny(E::NotFound)),
    row(A::AiAdviceFeedback, W::ServiceAccountNonMemberTokenReadApi, F::PrivateProject, deny(E::NotFound)),
    row(A::AiAdviceFeedback, W::ServiceAccountNonMemberTokenReadApi, F::InternalProject, deny(E::NotFound)),
    // A diagnosis's vote: an administrator's; anyone else 404.
    row(A::AiAdviceFeedback, W::Owner, F::Instance, deny(E::NotFound)),
    row(A::AiAdviceFeedback, W::AdminTokenApi, F::Instance, deny(E::NotFound)),
    row(A::AiAdviceFeedback, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::AiAdviceFeedback, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::AiAdviceFeedback, W::Reporter, F::DeletedProject, deny(E::PendingDeletion)),

    // --- AiReviewRerun ---
    row(A::AiReviewRerun, W::Developer, MR, Allow),
    row(A::AiReviewRerun, W::Maintainer, MR, Allow),
    row(A::AiReviewRerun, W::Admin, MR, Allow),
    row(A::AiReviewRerun, W::DevTokenApi, MR, Allow),
    row(A::AiReviewRerun, W::Reporter, MR, deny(E::Forbidden)),
    row(A::AiReviewRerun, W::Guest, MR, deny(E::Forbidden)),
    row(A::AiReviewRerun, W::AgentTokenApi, MR, deny(E::Forbidden)),
    row(A::AiReviewRerun, W::AgentMaintainerTokenApi, MR, deny(E::Forbidden)),
    row(A::AiReviewRerun, W::ServiceAccountTokenApi, MR, deny(E::Forbidden)),
    row(A::AiReviewRerun, W::BotTokenApi, MR, deny(E::Forbidden)),
    row(A::AiReviewRerun, W::MaintainerTokenReadApi, MR, deny(E::InsufficientScope)),
    row(A::AiReviewRerun, W::NonMember, MR, deny(E::NotFound)),
    // A visible merge request without a role: 403, not 404.
    row(A::AiReviewRerun, W::NonMember, INTERNAL_MR, deny(E::Forbidden)),
    row(A::AiReviewRerun, W::Anonymous, MR, deny(E::Unauthenticated)),
    row(A::AiReviewRerun, W::Disabled, MR, deny(E::AccountDisabled)),

    // --- LlmSettingsManage (`llm.review`) ---
    row(A::LlmSettingsManage, W::Maintainer, F::PrivateProject, Allow),
    row(A::LlmSettingsManage, W::Owner, F::PrivateProject, Allow),
    row(A::LlmSettingsManage, W::Admin, F::PrivateProject, Allow),
    row(A::LlmSettingsManage, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::LlmSettingsManage, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    // llm_settings_denied_for_token_session: a token, a person's too.
    row(A::LlmSettingsManage, W::MaintainerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::LlmSettingsManage, W::OwnerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::LlmSettingsManage, W::AdminTokenAdmin, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::LlmSettingsManage, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::LlmSettingsManage, W::AgentOwnerTokenApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::LlmSettingsManage, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::LlmSettingsManage, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    row(A::LlmSettingsManage, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::LlmSettingsManage, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),

    // --- OpsDiagnosisRead ---
    row(A::OpsDiagnosisRead, W::Admin, F::Instance, Allow),
    row(A::OpsDiagnosisRead, W::AdminTokenAdmin, F::Instance, Allow),
    row(A::OpsDiagnosisRead, W::AdminTokenReadApiAdmin, F::Instance, Allow),
    // Plan 19 §11 Q6: an administrator's agent token reads it (the
    // confidential ones are withheld).
    row(A::OpsDiagnosisRead, W::AgentAdminTokenAdmin, F::Instance, Allow),
    // diagnosis_read_denied_for_non_admin: 404, as if there were none.
    row(A::OpsDiagnosisRead, W::Developer, F::Instance, deny(E::NotFound)),
    row(A::OpsDiagnosisRead, W::Owner, F::Instance, deny(E::NotFound)),
    row(A::OpsDiagnosisRead, W::AdminTokenApi, F::Instance, deny(E::NotFound)),
    row(A::OpsDiagnosisRead, W::AgentOwnerTokenApi, F::Instance, deny(E::NotFound)),
    row(A::OpsDiagnosisRead, W::DevTokenUserEmail, F::Instance, deny(E::InsufficientScope)),
    row(A::OpsDiagnosisRead, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    row(A::OpsDiagnosisRead, W::DisabledAdmin, F::Instance, deny(E::AccountDisabled)),

    // --- LlmPause / LlmResume ---
    row(A::LlmPause, W::Admin, F::Instance, Allow),
    row(A::LlmPause, W::AdminTokenAdmin, F::Instance, Allow),
    row(A::LlmPause, W::AgentAdminTokenAdmin, F::Instance, deny(E::Forbidden)),
    row(A::LlmPause, W::Owner, F::Instance, deny(E::Forbidden)),
    row(A::LlmPause, W::AdminTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::LlmPause, W::AdminTokenReadApiAdmin, F::Instance, deny(E::InsufficientScope)),
    row(A::LlmPause, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    row(A::LlmPause, W::DisabledAdmin, F::Instance, deny(E::AccountDisabled)),
    row(A::LlmResume, W::Admin, F::Instance, Allow),
    row(A::LlmResume, W::AdminTokenAdmin, F::Instance, Allow),
    row(A::LlmResume, W::AgentAdminTokenAdmin, F::Instance, deny(E::Forbidden)),
    row(A::LlmResume, W::Owner, F::Instance, deny(E::Forbidden)),
    row(A::LlmResume, W::AdminTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::LlmResume, W::AdminTokenReadApiAdmin, F::Instance, deny(E::InsufficientScope)),
    row(A::LlmResume, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    row(A::LlmResume, W::DisabledAdmin, F::Instance, deny(E::AccountDisabled)),
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use forge_core::llm::InputClass;
    use forge_errors::ForgeError;

    use super::*;
    use crate::action::ActAsRule;
    use crate::llm::{authorize_advice, AdviceReader};
    use crate::matrix_rows_act_as::{via, Assert, ViaToken, BOUND_MR};
    use crate::model::Role;

    fn decide(action: A, actor: W, fixture: F) -> Result<(), ForgeError> {
        crate::authorize(&actor.principal(), action, &fixture.resource(actor))
    }

    fn reason(e: &ForgeError) -> String {
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn refused(action: A, actor: W, fixture: F) -> (E, String) {
        let e = decide(action, actor, fixture).expect_err("denied");
        (e.code(), reason(&e))
    }

    #[test]
    fn feedback_denied_for_agent_token() {
        for (actor, fixture) in [
            (W::AgentTokenApi, F::PrivateProject),
            (W::AgentTokenApi, MR),
            (W::AgentOwnerTokenApi, F::PrivateProject),
            (W::AgentAdminTokenAdmin, F::Instance),
        ] {
            assert_eq!(
                refused(A::AiAdviceFeedback, actor, fixture),
                (E::Forbidden, "agent_token".to_owned()),
                "{actor:?}"
            );
        }
        // A service account and a bot: refused too.
        assert_eq!(
            refused(
                A::AiAdviceFeedback,
                W::ServiceAccountTokenApi,
                F::PrivateProject
            )
            .1,
            "service_account"
        );
        assert_eq!(
            refused(A::AiAdviceFeedback, W::BotTokenApi, F::PrivateProject).1,
            "bot"
        );
        // The same role's person, by session and by token, votes.
        assert!(decide(A::AiAdviceFeedback, W::Developer, MR).is_ok());
        assert!(decide(A::AiAdviceFeedback, W::DevTokenApi, MR).is_ok());
        assert!(decide(A::AiAdviceFeedback, W::AdminTokenAdmin, F::Instance).is_ok());
    }

    #[test]
    fn llm_settings_denied_for_token_session() {
        for actor in [
            W::MaintainerTokenApi,
            W::OwnerTokenApi,
            W::AdminTokenAdmin,
            W::AgentMaintainerTokenApi,
        ] {
            assert_eq!(
                refused(A::LlmSettingsManage, actor, F::PrivateProject),
                (E::InsufficientScope, "session_required".to_owned()),
                "{actor:?}"
            );
        }
        // A service account and a bot even in a session; the auto-merge
        // principal never.
        assert_eq!(
            refused(
                A::LlmSettingsManage,
                W::ServiceAccountAdminSession,
                F::PrivateProject
            )
            .1,
            "service_account"
        );
        assert_eq!(
            refused(A::LlmSettingsManage, W::BotAdminSession, F::PrivateProject).1,
            "bot"
        );
        assert_eq!(
            refused(A::LlmSettingsManage, W::AutoMerge, F::PrivateProject).1,
            "auto_merge_not_allowed"
        );
        assert!(decide(A::LlmSettingsManage, W::Maintainer, F::PrivateProject).is_ok());
        assert!(decide(A::LlmSettingsManage, W::Developer, F::PrivateProject).is_err());
    }

    #[test]
    fn diagnosis_read_denied_for_non_admin() {
        for actor in [
            W::Developer,
            W::Owner,
            W::AdminTokenApi,
            W::AgentOwnerTokenApi,
            W::ServiceAccountOwnerTokenApi,
            W::BotOwnerTokenApi,
        ] {
            assert_eq!(
                refused(A::OpsDiagnosisRead, actor, F::Instance),
                (E::NotFound, "admin_only".to_owned()),
                "{actor:?}"
            );
            // A vote on a diagnosis says no more.
            assert_eq!(
                refused(A::AiAdviceFeedback, actor, F::Instance).0,
                E::NotFound,
                "{actor:?}"
            );
        }
        assert!(decide(A::OpsDiagnosisRead, W::Admin, F::Instance).is_ok());
        assert!(decide(A::OpsDiagnosisRead, W::AdminTokenReadApiAdmin, F::Instance).is_ok());
        assert!(decide(A::OpsDiagnosisRead, W::AgentAdminTokenAdmin, F::Instance).is_ok());
        // Acting for a person lends no administrator right.
        let acting = via(None, ViaToken::Reader, Assert::None);
        assert_eq!(
            refused(A::OpsDiagnosisRead, acting, F::Instance),
            (E::Forbidden, "act_as_not_allowed".to_owned())
        );
    }

    #[test]
    fn confidential_advice_withheld_for_agent() {
        let reader = |action: A, actor: W, fixture: F, declared: bool| {
            authorize_advice(
                &actor.principal(),
                action,
                &fixture.resource(actor),
                declared,
            )
            .expect("the subject is readable")
        };
        let withheld = |r: AdviceReader| {
            r == AdviceReader::Agent && !InputClass::Confidential.shown_to_agents()
        };
        // The agents read the merge request, so they are told the advice
        // exists — never its confidential words.
        for actor in [
            W::AgentTokenReadApi,
            W::AgentTokenApi,
            W::ServiceAccountTokenReadApi,
        ] {
            let r = reader(A::MergeRequestRead, actor, MR, false);
            assert_eq!(r, AdviceReader::Agent, "{actor:?}");
            assert!(withheld(r), "{actor:?}");
        }
        // Acting for a person is a tool, not the person (ADR-0039 6).
        let acting = via(Some(Role::Developer), ViaToken::Reader, Assert::None);
        let r = authorize_advice(
            &acting.principal(),
            A::MergeRequestRead,
            &BOUND_MR.resource(acting),
            false,
        )
        .expect("the person and the service account read it");
        assert!(withheld(r));
        // An administrator's agent token reads the diagnosis, internal only.
        let r = reader(
            A::OpsDiagnosisRead,
            W::AgentAdminTokenAdmin,
            F::Instance,
            false,
        );
        assert!(withheld(r));
        assert!(InputClass::Internal.shown_to_agents());
        // A person's token declared an agent audience counts as an agent.
        assert!(withheld(reader(
            A::MergeRequestRead,
            W::DevTokenApi,
            MR,
            true
        )));
        // People see the words.
        for actor in [W::Developer, W::DevTokenApi, W::Reporter] {
            assert_eq!(
                reader(A::MergeRequestRead, actor, MR, false),
                AdviceReader::Person,
                "{actor:?}"
            );
        }
        assert_eq!(
            reader(A::OpsDiagnosisRead, W::Admin, F::Instance, false),
            AdviceReader::Person
        );
        // A caller who may not read the subject learns nothing.
        let e = authorize_advice(
            &W::NonMember.principal(),
            A::MergeRequestRead,
            &MR.resource(W::NonMember),
            false,
        )
        .expect_err("invisible");
        assert_eq!(e.code(), E::NotFound);
        // Only the subject reads and the vote decide an advice.
        let e = authorize_advice(
            &W::Developer.principal(),
            A::MergeRequestComment,
            &MR.resource(W::Developer),
            false,
        )
        .expect_err("not an advice action");
        assert_eq!(
            (e.code(), reason(&e)),
            (E::Internal, "advice_action_invalid".to_owned())
        );
    }

    #[test]
    fn act_as_cannot_rerun_review() {
        assert_eq!(A::AiReviewRerun.act_as_rule(), ActAsRule::Denied);
        // Even a Maintainer person with an approve assertion on the merge
        // request: no write of the LLM is acted for.
        for person in [Role::Developer, Role::Maintainer] {
            for (token, assertion) in [
                (ViaToken::Approver, Assert::Approve),
                (ViaToken::Binder, Assert::None),
            ] {
                let acting = via(Some(person), token, assertion);
                assert_eq!(
                    refused(A::AiReviewRerun, acting, BOUND_MR),
                    (E::Forbidden, "act_as_not_allowed".to_owned()),
                    "{person:?} {token:?}"
                );
            }
        }
        // The same person in a session reruns it.
        assert!(decide(A::AiReviewRerun, W::Developer, MR).is_ok());
        // Nothing of the LLM is acted for; the act-as reads stay reads.
        for a in [
            A::AiAdviceFeedback,
            A::AiReviewRerun,
            A::LlmSettingsManage,
            A::OpsDiagnosisRead,
            A::LlmPause,
            A::LlmResume,
        ] {
            assert_eq!(a.act_as_rule(), ActAsRule::Denied, "{a:?}");
        }
        let reads: BTreeSet<A> = A::ALL
            .iter()
            .copied()
            .filter(|a| a.act_as_rule() == ActAsRule::ReadWithoutAssertion)
            .collect();
        let expected: BTreeSet<A> = [
            A::ProjectRead,
            A::MergeRequestRead,
            A::MergeRequestList,
            A::PipelineRead,
            A::DeployRead,
        ]
        .into_iter()
        .collect();
        assert_eq!(reads, expected);
    }
}
