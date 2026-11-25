//! The permission matrix rows of the agent marketplace and builder (plan
//! 16 §4 A4, ADR-0032 Amendment A4, ADR-0025 Amendment A4).
//!
//! - `AgentPartRead` / `AgentPartList` / `AgentKeyRead`: every signed-in
//!   account on the instance (`read_api`); anonymous 401.
//! - `AgentPartPublish` / `AgentPartPromote`: a Maintainer of the
//!   publishing group, a person, a web session only, never one of the
//!   people behind the tag (`FG-AGENT-0009`, administrators included); a
//!   part with the `network` capability an administrator's.
//! - `AgentPartRevoke`: a Maintainer of the publishing group or an
//!   administrator, a person, a web session only.
//! - `AgentBuilderRecommend`: `RepoRead` on a whole copy of the default
//!   tip (an AI agent audience only when it is measured open).
//! - `AgentBuilderDryRun`: a Developer, an agent included;
//!   `AgentBuilderCompile` (the merge request): a Developer, a person.
//! - `RuleInjectLesson`: kanflow acting for a Developer person
//!   (`ActAsRule::ProposeWithoutAssertion`), the lesson confirmed
//!   (`FG-AGENT-0010`); every direct caller `reason=act_as_required`.
//!
//! The act-as and CI denies of every other action are generated from
//! `act_as_rule` / `ci_rule` (`matrix_rows_act_as`, `matrix_rows_ci`);
//! the service account and bot denies of the people-only actions are
//! here (`every_people_only_action_denies_service_accounts_and_bots`).

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Expect::Allow, Fixture as F, Row};
use crate::matrix_rows_act_as::{via, Assert, Via, ViaToken};
use crate::model::{CopyMark as M, Role};

#[cfg(test)]
use crate::matrix::{ME, OTHER};
#[cfg(test)]
use crate::model::{
    ActAsView, AgentPartView, AgentResource, GroupSight, LessonView, Resource, ViaView,
};

/// Who pushed the tag of a part row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pusher {
    Other,
    Me,
    Unknown,
}

/// What an agent row acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scene {
    /// A part version of the publishing group, the actor's role on the
    /// group; `pusher`: who pushed the tag; `authored`: the caller is one
    /// of the other people behind the tag;
    /// `network`: the version declares the capability; `private`: the
    /// group's chain is private (otherwise internal).
    Part {
        pusher: Pusher,
        authored: bool,
        network: bool,
        private: bool,
    },
    /// A lesson for a private project (the actor's role on it);
    /// `wrapped`: as an act-as request reaches it (kanflow's service
    /// account a Reporter on the project, `bound` to the request's
    /// integration), otherwise the plain resource a direct call builds.
    Lesson {
        confirmed: bool,
        wrapped: bool,
        bound: bool,
    },
    /// A confirmed lesson on a bound project where kanflow's service
    /// account holds no role.
    LessonViaNotMember,
}

#[cfg(test)]
impl Scene {
    pub(crate) fn resource(self, role: Option<Role>) -> Resource {
        match self {
            Scene::Part {
                pusher,
                authored,
                network,
                private,
            } => Resource::Agent(AgentResource::Part(AgentPartView {
                tag_pusher: match pusher {
                    Pusher::Other => Some(OTHER),
                    Pusher::Me => Some(ME),
                    Pusher::Unknown => None,
                },
                group_role: role,
                group_sight: if private {
                    GroupSight::ROLE_ONLY
                } else {
                    GroupSight::INTERNAL
                },
                tag_authors: if authored {
                    vec![OTHER, ME]
                } else {
                    vec![OTHER]
                },
                network,
            })),
            Scene::LessonViaNotMember => {
                let project = crate::matrix_rows_act_as::project_view(
                    crate::matrix_review::MrProject::Private,
                    role,
                );
                Resource::ActAs(Box::new(ActAsView {
                    inner: Resource::Agent(AgentResource::Lesson(LessonView::new(project, true))),
                    via: ViaView {
                        role: None,
                        bound: true,
                    },
                    subject: None,
                    binding: None,
                }))
            }
            Scene::Lesson {
                confirmed,
                wrapped,
                bound,
            } => {
                let project = crate::matrix_rows_act_as::project_view(
                    crate::matrix_review::MrProject::Private,
                    role,
                );
                let lesson =
                    Resource::Agent(AgentResource::Lesson(LessonView::new(project, confirmed)));
                if !wrapped {
                    return lesson;
                }
                Resource::ActAs(Box::new(ActAsView {
                    inner: lesson,
                    via: ViaView {
                        role: Some(Role::Reporter),
                        bound,
                    },
                    subject: None,
                    binding: None,
                }))
            }
        }
    }
}

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

const fn part(authored: bool, network: bool, private: bool) -> F {
    F::Agent(Scene::Part {
        pusher: Pusher::Other,
        authored,
        network,
        private,
    })
}

const fn lesson(confirmed: bool, wrapped: bool, bound: bool) -> F {
    F::Agent(Scene::Lesson {
        confirmed,
        wrapped,
        bound,
    })
}

/// Another's tag, no network, an internal group.
const PART: F = part(false, false, false);
/// The caller is behind the tag.
const OWN_PART: F = part(true, false, false);
/// A part that uses the network.
const NETWORK_PART: F = part(false, true, false);
/// A part of a private group.
const PRIVATE_PART: F = part(false, false, true);
/// The caller pushed the tag (another person tagged and committed).
const PUSHED_BY_ME: F = F::Agent(Scene::Part {
    pusher: Pusher::Me,
    authored: false,
    network: false,
    private: false,
});
/// Nobody known pushed the tag.
const PUSHER_UNKNOWN: F = F::Agent(Scene::Part {
    pusher: Pusher::Unknown,
    authored: false,
    network: false,
    private: false,
});

/// A confirmed lesson as kanflow's act-as request reaches it.
const LESSON: F = lesson(true, true, true);
/// The same, not confirmed.
const UNCONFIRMED: F = lesson(false, true, true);
/// The same, the project not bound to the request's integration.
const UNBOUND: F = lesson(true, true, false);
/// The same, kanflow's service account no member of the project.
const NOT_MEMBER: F = F::Agent(Scene::LessonViaNotMember);
/// A confirmed lesson a direct call (no act-as) reaches.
const DIRECT: F = lesson(true, false, true);

const DEV: Option<Role> = Some(Role::Developer);
/// Kanflow's binding token (`read_api` + `api` + `act_as`) for a person.
const fn kanflow(person: Option<Role>) -> W {
    via(person, ViaToken::Binder, Assert::None)
}
/// A Developer person through kanflow.
const KANFLOW_DEV: W = kanflow(DEV);

#[rustfmt::skip]
pub(crate) const AGENT_ROWS: &[Row] = &[
    // --- AgentPartRead / AgentPartList / AgentKeyRead: signed in ---
    row(A::AgentPartRead, W::NonMember, F::Instance, Allow),
    row(A::AgentPartRead, W::NonMemberTokenReadApi, F::Instance, Allow),
    row(A::AgentPartRead, W::AgentTokenReadApi, F::Instance, Allow),
    row(A::AgentPartRead, W::BotNonMemberTokenReadApi, F::Instance, Allow),
    row(A::AgentPartRead, W::ServiceAccountNonMemberTokenReadApi, F::Instance, Allow),
    row(A::AgentPartRead, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    row(A::AgentPartRead, W::Disabled, F::Instance, deny(E::AccountDisabled)),
    row(A::AgentPartRead, W::DevTokenUserEmail, F::Instance, deny(E::InsufficientScope)),
    row(A::AgentPartRead, W::DevTokenReadRepository, F::Instance, deny(E::InsufficientScope)),
    row(A::AgentPartList, W::NonMember, F::Instance, Allow),
    row(A::AgentPartList, W::AgentTokenReadApi, F::Instance, Allow),
    row(A::AgentPartList, W::BotNonMemberTokenReadApi, F::Instance, Allow),
    row(A::AgentPartList, W::ServiceAccountNonMemberTokenReadApi, F::Instance, Allow),
    row(A::AgentPartList, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    row(A::AgentPartList, W::Disabled, F::Instance, deny(E::AccountDisabled)),
    row(A::AgentPartList, W::DevTokenUserEmail, F::Instance, deny(E::InsufficientScope)),
    row(A::AgentKeyRead, W::NonMember, F::Instance, Allow),
    row(A::AgentKeyRead, W::NonMemberTokenReadApi, F::Instance, Allow),
    row(A::AgentKeyRead, W::BotNonMemberTokenReadApi, F::Instance, Allow),
    row(A::AgentKeyRead, W::AgentTokenReadApi, F::Instance, Allow),
    row(A::AgentKeyRead, W::ServiceAccountNonMemberTokenReadApi, F::Instance, Allow),
    row(A::AgentKeyRead, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    row(A::AgentKeyRead, W::Disabled, F::Instance, deny(E::AccountDisabled)),
    row(A::AgentKeyRead, W::DevTokenUserEmail, F::Instance, deny(E::InsufficientScope)),
    // A part's catalog is no resource of a project or a part.
    row(A::AgentPartRead, W::Developer, PART, deny(E::Internal)),

    // --- AgentPartPublish ---
    row(A::AgentPartPublish, W::Maintainer, PART, Allow),
    row(A::AgentPartPublish, W::Owner, PART, Allow),
    row(A::AgentPartPublish, W::Maintainer, PRIVATE_PART, Allow),
    row(A::AgentPartPublish, W::Admin, PART, Allow),
    row(A::AgentPartPublish, W::Admin, NETWORK_PART, Allow),
    row(A::AgentPartPublish, W::Admin, PRIVATE_PART, Allow),
    // publish_denied_for_tag_author: the two-person rule, administrators
    // included.
    row(A::AgentPartPublish, W::Maintainer, OWN_PART, deny(E::PublishSelfApproval)),
    row(A::AgentPartPublish, W::Owner, OWN_PART, deny(E::PublishSelfApproval)),
    row(A::AgentPartPublish, W::Admin, OWN_PART, deny(E::PublishSelfApproval)),
    row(A::AgentPartPublish, W::Maintainer, PUSHED_BY_ME, deny(E::PublishSelfApproval)),
    row(A::AgentPartPublish, W::Admin, PUSHED_BY_ME, deny(E::PublishSelfApproval)),
    // Fails closed: nobody known pushed the tag.
    row(A::AgentPartPublish, W::Maintainer, PUSHER_UNKNOWN, deny(E::Forbidden)),
    row(A::AgentPartPublish, W::Admin, PUSHER_UNKNOWN, deny(E::Forbidden)),
    row(A::AgentPartPromote, W::Maintainer, PUSHER_UNKNOWN, deny(E::Forbidden)),
    row(A::AgentPartPromote, W::Maintainer, PUSHED_BY_ME, deny(E::PublishSelfApproval)),
    // A part that uses the network: an administrator's.
    row(A::AgentPartPublish, W::Maintainer, NETWORK_PART, deny(E::Forbidden)),
    row(A::AgentPartPublish, W::Owner, NETWORK_PART, deny(E::Forbidden)),
    row(A::AgentPartPublish, W::Developer, PART, deny(E::Forbidden)),
    row(A::AgentPartPublish, W::Reporter, PART, deny(E::Forbidden)),
    row(A::AgentPartPublish, W::NonMember, PART, deny(E::Forbidden)),
    // A private group without a role: as if there were none.
    row(A::AgentPartPublish, W::NonMember, PRIVATE_PART, deny(E::NotFound)),
    // publish_denied_for_agent_token: a token (an agent's or a person's,
    // administrators' included) is refused before the part is looked at.
    row(A::AgentPartPublish, W::AgentMaintainerTokenApi, PART, deny(E::InsufficientScope)),
    row(A::AgentPartPublish, W::AgentAdminTokenAdmin, PART, deny(E::InsufficientScope)),
    row(A::AgentPartPublish, W::MaintainerTokenApi, PART, deny(E::InsufficientScope)),
    row(A::AgentPartPublish, W::AdminTokenAdmin, PART, deny(E::InsufficientScope)),
    row(A::AgentPartPublish, W::ServiceAccountMaintainerTokenApi, PART, deny(E::InsufficientScope)),
    row(A::AgentPartPublish, W::BotMaintainerTokenApi, PART, deny(E::InsufficientScope)),
    // A service account's or a bot's session: people only.
    row(A::AgentPartPublish, W::ServiceAccountAdminSession, PART, deny(E::Forbidden)),
    row(A::AgentPartPublish, W::BotAdminSession, PART, deny(E::Forbidden)),
    row(A::AgentPartPublish, W::Anonymous, PART, deny(E::Unauthenticated)),
    row(A::AgentPartPublish, W::Disabled, PART, deny(E::AccountDisabled)),
    row(A::AgentPartPublish, W::DisabledAdmin, PART, deny(E::AccountDisabled)),
    // A part version is no instance or project action.
    row(A::AgentPartPublish, W::Admin, F::Instance, deny(E::Internal)),

    // --- AgentPartPromote (beta → stable): as publishing ---
    row(A::AgentPartPromote, W::Maintainer, PART, Allow),
    row(A::AgentPartPromote, W::Owner, PART, Allow),
    row(A::AgentPartPromote, W::Admin, NETWORK_PART, Allow),
    row(A::AgentPartPromote, W::Maintainer, OWN_PART, deny(E::PublishSelfApproval)),
    row(A::AgentPartPromote, W::Admin, OWN_PART, deny(E::PublishSelfApproval)),
    row(A::AgentPartPromote, W::Maintainer, NETWORK_PART, deny(E::Forbidden)),
    row(A::AgentPartPromote, W::Developer, PART, deny(E::Forbidden)),
    row(A::AgentPartPromote, W::Guest, PART, deny(E::Forbidden)),
    row(A::AgentPartPromote, W::NonMember, PRIVATE_PART, deny(E::NotFound)),
    row(A::AgentPartPromote, W::AgentMaintainerTokenApi, PART, deny(E::InsufficientScope)),
    row(A::AgentPartPromote, W::MaintainerTokenApi, PART, deny(E::InsufficientScope)),
    row(A::AgentPartPromote, W::ServiceAccountAdminSession, PART, deny(E::Forbidden)),
    row(A::AgentPartPromote, W::BotAdminSession, PART, deny(E::Forbidden)),
    row(A::AgentPartPromote, W::Anonymous, PART, deny(E::Unauthenticated)),

    // --- AgentPartRevoke ---
    row(A::AgentPartRevoke, W::Maintainer, PART, Allow),
    row(A::AgentPartRevoke, W::Owner, PART, Allow),
    // The author may withdraw their own version; any capability.
    row(A::AgentPartRevoke, W::Maintainer, OWN_PART, Allow),
    row(A::AgentPartRevoke, W::Maintainer, NETWORK_PART, Allow),
    row(A::AgentPartRevoke, W::Admin, PART, Allow),
    row(A::AgentPartRevoke, W::Admin, PRIVATE_PART, Allow),
    row(A::AgentPartRevoke, W::Developer, PART, deny(E::Forbidden)),
    row(A::AgentPartRevoke, W::NonMember, PRIVATE_PART, deny(E::NotFound)),
    row(A::AgentPartRevoke, W::AgentMaintainerTokenApi, PART, deny(E::InsufficientScope)),
    row(A::AgentPartRevoke, W::AgentAdminTokenAdmin, PART, deny(E::InsufficientScope)),
    row(A::AgentPartRevoke, W::MaintainerTokenApi, PART, deny(E::InsufficientScope)),
    row(A::AgentPartRevoke, W::AdminTokenAdmin, PART, deny(E::InsufficientScope)),
    row(A::AgentPartRevoke, W::ServiceAccountAdminSession, PART, deny(E::Forbidden)),
    row(A::AgentPartRevoke, W::BotAdminSession, PART, deny(E::Forbidden)),
    row(A::AgentPartRevoke, W::Anonymous, PART, deny(E::Unauthenticated)),
    row(A::AgentPartRevoke, W::DisabledAdmin, PART, deny(E::AccountDisabled)),
    row(A::AgentPartRevoke, W::Disabled, PART, deny(E::AccountDisabled)),

    // --- AgentBuilderRecommend: as a whole copy of the default tip ---
    row(A::AgentBuilderRecommend, W::Reporter, F::RepoCopy(M::Confidential), Allow),
    row(A::AgentBuilderRecommend, W::DevTokenApi, F::RepoCopy(M::Confidential), Allow),
    row(A::AgentBuilderRecommend, W::NonMember, F::InternalRepoCopy(M::Undecided), Allow),
    row(A::AgentBuilderRecommend, W::AgentTokenReadApi, F::RepoCopy(M::Open), Allow),
    row(A::AgentBuilderRecommend, W::BotNonMemberTokenReadApi, F::InternalRepoCopy(M::Open), Allow),
    row(A::AgentBuilderRecommend, W::Admin, F::RepoCopy(M::Confidential), Allow),
    // An agent only when the specs' tip is measured open.
    row(A::AgentBuilderRecommend, W::AgentTokenReadApi, F::RepoCopy(M::Confidential), deny(E::RepositoryCopyWithheld)),
    row(A::AgentBuilderRecommend, W::AgentTokenReadApi, F::RepoCopy(M::NotMeasured), deny(E::RepositoryCopyWithheld)),
    row(A::AgentBuilderRecommend, W::AgentTokenApi, F::RepoCopy(M::Undecided), deny(E::RepositoryCopyWithheld)),
    row(A::AgentBuilderRecommend, W::BotTokenApi, F::RepoCopy(M::Confidential), deny(E::RepositoryCopyWithheld)),
    row(A::AgentBuilderRecommend, W::ServiceAccountTokenReadApi, F::RepoCopy(M::Unborn), deny(E::RepositoryCopyWithheld)),
    // Not visible: 404.
    row(A::AgentBuilderRecommend, W::NonMember, F::RepoCopy(M::Open), deny(E::NotFound)),
    row(A::AgentBuilderRecommend, W::ServiceAccountNonMemberTokenReadApi, F::InternalRepoCopy(M::Open), deny(E::NotFound)),
    row(A::AgentBuilderRecommend, W::Guest, F::RepoCopy(M::Open), deny(E::Forbidden)),
    row(A::AgentBuilderRecommend, W::DevTokenReadRepository, F::RepoCopy(M::Open), deny(E::InsufficientScope)),
    row(A::AgentBuilderRecommend, W::Anonymous, F::RepoCopy(M::Open), deny(E::Unauthenticated)),
    row(A::AgentBuilderRecommend, W::Disabled, F::RepoCopy(M::Open), deny(E::AccountDisabled)),
    row(A::AgentBuilderRecommend, W::Developer, F::PrivateProject, deny(E::Internal)),

    // --- AgentBuilderDryRun: a Developer, an agent included ---
    row(A::AgentBuilderDryRun, W::Developer, F::PrivateProject, Allow),
    row(A::AgentBuilderDryRun, W::DevTokenApi, F::PrivateProject, Allow),
    row(A::AgentBuilderDryRun, W::AgentTokenApi, F::PrivateProject, Allow),
    row(A::AgentBuilderDryRun, W::BotTokenApi, F::PrivateProject, Allow),
    row(A::AgentBuilderDryRun, W::Admin, F::PrivateProject, Allow),
    row(A::AgentBuilderDryRun, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::AgentBuilderDryRun, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(A::AgentBuilderDryRun, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::AgentBuilderDryRun, W::AgentTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::AgentBuilderDryRun, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::AgentBuilderDryRun, W::Developer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::AgentBuilderDryRun, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),

    // --- AgentBuilderCompile (the merge request): a Developer, a person ---
    row(A::AgentBuilderCompile, W::Developer, F::PrivateProject, Allow),
    row(A::AgentBuilderCompile, W::DevTokenApi, F::PrivateProject, Allow),
    row(A::AgentBuilderCompile, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::AgentBuilderCompile, W::Admin, F::PrivateProject, Allow),
    // compile_mr_denied_for_agent: an agent never rewrites its own guard
    // rails, whatever its role.
    row(A::AgentBuilderCompile, W::AgentTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::AgentBuilderCompile, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::AgentBuilderCompile, W::AgentOwnerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::AgentBuilderCompile, W::AgentAdminTokenAdmin, F::PrivateProject, deny(E::Forbidden)),
    row(A::AgentBuilderCompile, W::ServiceAccountTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::AgentBuilderCompile, W::BotTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::AgentBuilderCompile, W::Reporter, F::PrivateProject, deny(E::Forbidden)),
    row(A::AgentBuilderCompile, W::Guest, F::PrivateProject, deny(E::Forbidden)),
    row(A::AgentBuilderCompile, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::AgentBuilderCompile, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::AgentBuilderCompile, W::Developer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::AgentBuilderCompile, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),

    // --- RuleInjectLesson: kanflow acting for a Developer person ---
    row(A::RuleInjectLesson, KANFLOW_DEV, LESSON, Allow),
    row(A::RuleInjectLesson, kanflow(Some(Role::Maintainer)), LESSON, Allow),
    // unconfirmed_lesson_refused.
    row(A::RuleInjectLesson, KANFLOW_DEV, UNCONFIRMED, deny(E::LessonNotConfirmed)),
    // The person's own role decides (no cap at the service account's
    // Reporter), and it must be Developer.
    row(A::RuleInjectLesson, kanflow(Some(Role::Reporter)), LESSON, deny(E::Forbidden)),
    row(A::RuleInjectLesson, kanflow(None), LESSON, deny(E::NotFound)),
    row(A::RuleInjectLesson, KANFLOW_DEV, UNBOUND, deny(E::NotFound)),
    // Bound, but kanflow's service account is no member: as for a read.
    row(A::RuleInjectLesson, KANFLOW_DEV, NOT_MEMBER, deny(E::NotFound)),
    // No assertion is taken (an unexpected one is refused), kanflow's
    // token needs `api`, the integration enabled, never an agent's token.
    row(A::RuleInjectLesson, via(DEV, ViaToken::Binder, Assert::BindShared), LESSON, deny(E::ActAsAssertionInvalid)),
    row(A::RuleInjectLesson, via(DEV, ViaToken::Reader, Assert::None), LESSON, deny(E::InsufficientScope)),
    row(A::RuleInjectLesson, W::ActAs(Via { person: DEV, token: ViaToken::Binder, assertion: Assert::None, enabled: false }), LESSON, deny(E::Forbidden)),
    row(A::RuleInjectLesson, via(DEV, ViaToken::Agent, Assert::None), LESSON, deny(E::Forbidden)),
    // Not acted for: 403 for every direct caller (代理でない).
    row(A::RuleInjectLesson, W::Developer, DIRECT, deny(E::Forbidden)),
    row(A::RuleInjectLesson, W::DevTokenApi, DIRECT, deny(E::Forbidden)),
    row(A::RuleInjectLesson, W::Admin, DIRECT, deny(E::Forbidden)),
    row(A::RuleInjectLesson, W::ServiceAccountTokenApi, DIRECT, deny(E::Forbidden)),
    row(A::RuleInjectLesson, W::AgentTokenApi, DIRECT, deny(E::Forbidden)),
    // An act-as resource without an act-as request.
    row(A::RuleInjectLesson, W::Developer, LESSON, deny(E::Forbidden)),
    row(A::RuleInjectLesson, W::NonMember, DIRECT, deny(E::NotFound)),
    row(A::RuleInjectLesson, W::Anonymous, DIRECT, deny(E::Unauthenticated)),
];

#[cfg(test)]
mod tests {
    use forge_errors::ForgeError;

    use super::*;
    use crate::action::ActAsRule;

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
    fn publish_denied_for_agent_token() {
        for action in [A::AgentPartPublish, A::AgentPartPromote, A::AgentPartRevoke] {
            for actor in [W::AgentMaintainerTokenApi, W::AgentAdminTokenAdmin] {
                assert_eq!(
                    refused(action, actor, PART),
                    (E::InsufficientScope, "session_required".to_owned()),
                    "{action:?} {actor:?}"
                );
            }
            // A service account's or a bot's session: people only.
            assert_eq!(
                refused(action, W::ServiceAccountAdminSession, PART),
                (E::Forbidden, "service_account".to_owned())
            );
            assert_eq!(
                refused(action, W::BotAdminSession, PART),
                (E::Forbidden, "bot".to_owned())
            );
            // Acting for a person, a CI job: never.
            assert_eq!(action.act_as_rule(), ActAsRule::Denied);
            assert_eq!(action.ci_rule(), crate::action::CiRule::Denied);
            assert!(action.is_session_only() && action.is_people_only());
        }
    }

    #[test]
    fn publish_denied_for_tag_author() {
        for action in [A::AgentPartPublish, A::AgentPartPromote] {
            for actor in [W::Maintainer, W::Owner, W::Admin] {
                assert_eq!(
                    refused(action, actor, OWN_PART),
                    (E::PublishSelfApproval, "tag_author".to_owned()),
                    "{action:?} {actor:?}"
                );
                assert!(decide(action, actor, PART).is_ok(), "{action:?} {actor:?}");
            }
        }
        for fixture in [OWN_PART, PUSHED_BY_ME] {
            assert_eq!(
                refused(A::AgentPartPublish, W::Maintainer, fixture).0,
                E::PublishSelfApproval
            );
        }
        // Fails closed when nobody known pushed the tag.
        assert_eq!(
            refused(A::AgentPartPublish, W::Admin, PUSHER_UNKNOWN),
            (E::Forbidden, "tag_pusher_unknown".to_owned())
        );
        // The author may still withdraw their own version.
        assert!(decide(A::AgentPartRevoke, W::Maintainer, OWN_PART).is_ok());
    }

    #[test]
    fn network_part_is_published_by_an_administrator_only() {
        assert_eq!(
            refused(A::AgentPartPublish, W::Owner, NETWORK_PART),
            (E::Forbidden, "network_capability".to_owned())
        );
        assert!(decide(A::AgentPartPublish, W::Admin, NETWORK_PART).is_ok());
    }

    #[test]
    fn compile_mr_denied_for_agent() {
        for actor in [
            W::AgentTokenApi,
            W::AgentMaintainerTokenApi,
            W::AgentOwnerTokenApi,
            W::AgentAdminTokenAdmin,
        ] {
            assert_eq!(
                refused(A::AgentBuilderCompile, actor, F::PrivateProject),
                (E::Forbidden, "agent_token".to_owned()),
                "{actor:?}"
            );
        }
        // The dry run is the agent's to see; the merge request a person's.
        assert!(decide(A::AgentBuilderDryRun, W::AgentTokenApi, F::PrivateProject).is_ok());
        assert!(decide(A::AgentBuilderCompile, W::DevTokenApi, F::PrivateProject).is_ok());
        assert_eq!(A::AgentBuilderCompile.act_as_rule(), ActAsRule::Denied);
    }

    #[test]
    fn recommend_withholds_confidential_specs_from_agents() {
        assert_eq!(
            refused(
                A::AgentBuilderRecommend,
                W::AgentTokenReadApi,
                F::RepoCopy(M::Confidential)
            ),
            (E::RepositoryCopyWithheld, "tip_confidential".to_owned())
        );
        assert!(decide(
            A::AgentBuilderRecommend,
            W::Reporter,
            F::RepoCopy(M::Confidential)
        )
        .is_ok());
        assert!(decide(
            A::AgentBuilderRecommend,
            W::AgentTokenReadApi,
            F::RepoCopy(M::Open)
        )
        .is_ok());
    }

    #[test]
    fn unconfirmed_lesson_refused() {
        assert_eq!(
            refused(A::RuleInjectLesson, KANFLOW_DEV, UNCONFIRMED),
            (E::LessonNotConfirmed, "lesson_not_confirmed".to_owned())
        );
        assert!(decide(A::RuleInjectLesson, KANFLOW_DEV, LESSON).is_ok());
    }

    #[test]
    fn lesson_is_injected_only_acting_for_a_person() {
        for actor in [W::Developer, W::DevTokenApi, W::Admin, W::AgentTokenApi] {
            assert_eq!(
                refused(A::RuleInjectLesson, actor, DIRECT),
                (E::Forbidden, "act_as_required".to_owned()),
                "{actor:?}"
            );
        }
        // A lesson built by a route is never marked as acted for.
        let plain = LessonView::new(
            crate::matrix_rows_act_as::project_view(crate::matrix_review::MrProject::Private, DEV),
            true,
        );
        assert!(!plain.is_acted_for());
        // The person's own role decides, not the service account's
        // Reporter (no min rule), and only on a bound project.
        assert_eq!(
            refused(A::RuleInjectLesson, kanflow(Some(Role::Reporter)), LESSON).0,
            E::Forbidden
        );
        assert_eq!(
            refused(A::RuleInjectLesson, KANFLOW_DEV, UNBOUND),
            (E::NotFound, "act_as_unbound".to_owned())
        );
    }

    #[test]
    fn propose_without_assertion_is_the_lesson_only() {
        let proposals: Vec<A> = A::ALL
            .iter()
            .copied()
            .filter(|a| a.act_as_rule() == ActAsRule::ProposeWithoutAssertion)
            .collect();
        assert_eq!(proposals, vec![A::RuleInjectLesson]);
        assert!(!A::RuleInjectLesson.is_people_only());
        // Another resource lacks the facts, whatever the person may do.
        let project =
            crate::matrix_rows_act_as::project_view(crate::matrix_review::MrProject::Private, DEV);
        let view = Resource::ActAs(Box::new(ActAsView {
            inner: Resource::Project(project),
            via: ViaView {
                role: Some(Role::Reporter),
                bound: true,
            },
            subject: None,
            binding: None,
        }));
        let e = crate::authorize(&KANFLOW_DEV.principal(), A::RuleInjectLesson, &view)
            .expect_err("no lesson");
        assert_eq!(
            (e.code(), reason(&e)),
            (E::Forbidden, "act_as_facts_missing".to_owned())
        );
    }
}
