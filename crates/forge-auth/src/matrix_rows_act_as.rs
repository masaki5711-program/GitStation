//! The permission matrix rows of acting for a person (M3 S2, ADR-0025 "権限
//! の表に S2 が足す行"):
//!
//! 1. generated: every action whose `act_as_rule()` is `Denied` is refused
//!    for a Developer person with a valid approve assertion on a bound
//!    project ([`generated_denies`], `act_as_denied_for_<action>`);
//! 2. allowed: the reads (both the person and the service account Reporter
//!    or above, a bound project, no assertion) and approve / unapprove (a
//!    Developer person who is not the author, `act_as` + `merge_approve`,
//!    the matching assertion); each with its person's session twin allowed
//!    (`act_as_allow_rows_have_an_allowed_session_twin`);
//! 3. denied changes: no assertion, the wrong intent, another merge
//!    request, the body's sha, the head, the target ref and the target's
//!    tip moved, the author, a Reporter, no `merge_approve`, no `act_as`, an
//!    unbound project (404), a disabled integration, an administrator
//!    without a membership (404), an agent's token;
//! 4. denied reads: the person or the service account cannot see the
//!    project (404), the service account's role is too low although the
//!    person's is enough (the min rule), an assertion on a read;
//! 5. the confirmation (`ActAsConfirmView`, `ActAsAssertionIssue`) and the
//!    claim (`ActAsAssertionClaim`).
//!
//! The feed's `act_as: true` rows stay in `matrix_rows_feed.rs` /
//! `matrix_rows_dev_status.rs`.

use forge_errors::ErrorCode as E;

use crate::action::{ActAsRule, Action as A};
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};
use crate::matrix_review::MrProject as P;
use crate::model::Role;

#[cfg(test)]
use crate::matrix::{ME, OTHER};
#[cfg(test)]
use crate::model::{
    AccountKind, ActAsCredential, ActAsSubject, ActAsView, AssertionFacts, AssertionSubject,
    MergeRequestEdit, Principal, ProjectView, Resource, Scope, UnresolvedReason, ViaView,
    Visibility,
};
#[cfg(test)]
use forge_core::{act_as::Intent, IntegrationId, MergeRequestId, MergeRequestState, ProjectId};

/// The service account's token in an act-as row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViaToken {
    /// `read_api` + `merge_approve` + `act_as` (kanflow's).
    Approver,
    /// `read_api` + `act_as`: reads only (approve lacks `merge_approve`).
    Reader,
    /// `read_api` + `merge_approve`, no `act_as`.
    NoActAs,
    /// [`ViaToken::Approver`] issued with `is_agent`.
    Agent,
    /// `read_api` + `api` + `act_as`: kanflow's token for the binding
    /// routes (M3 X3).
    Binder,
}

/// The assertion an act-as row's request carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Assert {
    None,
    /// For [`Scene`]'s merge request.
    Approve,
    Unapprove,
    /// An approve assertion for another merge request.
    ApproveOtherMr,
    /// M3 X3: `binding_set` of kanflow project `P1`, shared (not resolving).
    BindShared,
    /// `binding_set` of `P1`, resolving.
    BindResolving,
    /// `binding_set` of another kanflow project (`P2`), shared.
    BindOtherKanflow,
    /// `binding_remove` of `P1`.
    Unbind,
}

/// The caller of an act-as row: the person (`ME`, with `person` as role)
/// and the service account's request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Via {
    pub person: Option<Role>,
    pub token: ViaToken,
    pub assertion: Assert,
    pub enabled: bool,
}

/// What moved between the confirmation and the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drift {
    None,
    /// The body's `sha` is not the assertion's head.
    BodySha,
    /// A push since: the merge request's head is another.
    Head,
    /// The merge request was retargeted.
    Retargeted,
    /// The target branch moved.
    TargetTip,
}

/// What an act-as row acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Project,
    /// The merge request (`own`: the person is its author).
    Mr {
        own: bool,
    },
    /// Nothing at the path.
    Absent,
}

/// The resource of an act-as row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scene {
    /// The service account's membership role on the project.
    pub via_role: Option<Role>,
    pub bound: bool,
    pub project: P,
    pub target: Target,
    pub drift: Drift,
}

/// The confirmation of a merge request (another's unless `own`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConfirmScene {
    pub unapprove: bool,
    pub enabled: bool,
    pub bound: bool,
    pub own: bool,
    pub project: P,
}

#[cfg(test)]
const PROJECT: ProjectId = ProjectId::new(2);
#[cfg(test)]
const MR: MergeRequestId = MergeRequestId::new(20);
#[cfg(test)]
const HEAD: &str = "1111111111111111111111111111111111111111";
#[cfg(test)]
const TIP: &str = "2222222222222222222222222222222222222222";

#[cfg(test)]
fn name(raw: &str) -> forge_core::RefName {
    forge_core::RefName::parse(raw).expect("fixture ref names are valid")
}

#[cfg(test)]
impl Via {
    pub(crate) fn credential(self) -> ActAsCredential {
        let scopes = match self.token {
            ViaToken::Approver | ViaToken::Agent => {
                vec![Scope::ReadApi, Scope::MergeApprove, Scope::ActAs]
            }
            ViaToken::Reader => vec![Scope::ReadApi, Scope::ActAs],
            ViaToken::NoActAs => vec![Scope::ReadApi, Scope::MergeApprove],
            ViaToken::Binder => vec![Scope::ReadApi, Scope::Api, Scope::ActAs],
        };
        let assertion = |intent, subject| AssertionFacts {
            id: 9,
            public_id: "00000000000000a5".to_owned(),
            intent,
            project: PROJECT,
            subject,
        };
        let facts = |intent, mr| {
            assertion(
                intent,
                AssertionSubject::MergeRequest {
                    merge_request: mr,
                    head_sha: HEAD.to_owned(),
                    target_ref: name("refs/heads/main"),
                    target_sha: TIP.to_owned(),
                },
            )
        };
        let bind = |intent, external: &str, resolves| {
            assertion(
                intent,
                AssertionSubject::Binding {
                    external_project_id: external.to_owned(),
                    resolves_work_items: resolves,
                },
            )
        };
        ActAsCredential {
            via: OTHER,
            via_kind: AccountKind::ServiceAccount,
            via_token: 77,
            via_scopes: scopes,
            via_agent: self.token == ViaToken::Agent,
            integration: IntegrationId::new(5),
            integration_enabled: self.enabled,
            assertion: match self.assertion {
                Assert::None => None,
                Assert::Approve => Some(facts(Intent::Approve, MR)),
                Assert::Unapprove => Some(facts(Intent::Unapprove, MR)),
                Assert::ApproveOtherMr => Some(facts(Intent::Approve, MergeRequestId::new(21))),
                Assert::BindShared => Some(bind(Intent::BindingSet, "P1", Some(false))),
                Assert::BindResolving => Some(bind(Intent::BindingSet, "P1", Some(true))),
                Assert::BindOtherKanflow => Some(bind(Intent::BindingSet, "P2", Some(false))),
                Assert::Unbind => Some(bind(Intent::BindingRemove, "P1", None)),
            },
            claimed_mac: "c".repeat(64),
            claimed_key_id: 1,
        }
    }

    pub(crate) fn principal(self) -> Principal {
        Principal::act_as(ME, true, AccountKind::Human, self.credential())
    }
}

#[cfg(test)]
pub(crate) fn project_view(project: P, role: Option<Role>) -> ProjectView {
    let (visibility, pending_deletion) = match project {
        P::Private => (Visibility::Private, false),
        P::Internal => (Visibility::Internal, false),
        P::Deleted => (Visibility::Private, true),
    };
    ProjectView {
        visibility,
        role,
        pending_deletion,
    }
}

#[cfg(test)]
fn merge_request(project: ProjectView, own: bool) -> Resource {
    Resource::MergeRequest {
        project,
        author: if own { ME } else { OTHER },
        state: MergeRequestState::Open,
        draft: false,
        source_branch: name("refs/heads/feature"),
        target_branch: name("refs/heads/main"),
        edit: MergeRequestEdit::default(),
        target_protection: None,
    }
}

#[cfg(test)]
impl Scene {
    /// The person's view (`role`: the actor's) wrapped with the service
    /// account's facts.
    pub(crate) fn resource(self, role: Option<Role>) -> Resource {
        let view = project_view(self.project, role);
        let (inner, subject) = match self.target {
            Target::Absent => return Resource::Unresolved(UnresolvedReason::Absent),
            Target::Project => (Resource::Project(view), None),
            Target::Mr { own } => {
                let head = match self.drift {
                    Drift::Head => "3333333333333333333333333333333333333333",
                    Drift::None | Drift::BodySha | Drift::Retargeted | Drift::TargetTip => HEAD,
                };
                let requested = match self.drift {
                    Drift::BodySha => "4444444444444444444444444444444444444444",
                    Drift::None | Drift::Head | Drift::Retargeted | Drift::TargetTip => HEAD,
                };
                let target = match self.drift {
                    Drift::Retargeted => "refs/heads/release",
                    Drift::None | Drift::BodySha | Drift::Head | Drift::TargetTip => {
                        "refs/heads/main"
                    }
                };
                let tip = match self.drift {
                    Drift::TargetTip => "5555555555555555555555555555555555555555",
                    Drift::None | Drift::BodySha | Drift::Head | Drift::Retargeted => TIP,
                };
                (
                    merge_request(view, own),
                    Some(ActAsSubject {
                        project: PROJECT,
                        merge_request: MR,
                        head_sha: Some(head.to_owned()),
                        target_ref: name(target),
                        target_sha: Some(tip.to_owned()),
                        requested_sha: Some(requested.to_owned()),
                    }),
                )
            }
        };
        Resource::ActAs(Box::new(ActAsView {
            binding: None,
            inner,
            via: ViaView {
                role: self.via_role,
                bound: self.bound,
            },
            subject,
        }))
    }

    /// The person's own view (the session twin decides on it).
    pub(crate) fn person_resource(self, role: Option<Role>) -> Resource {
        let resource = self.resource(role);
        if let Resource::ActAs(view) = &resource {
            return view.inner.clone(); // clone: the twin decides on its own copy
        }
        resource
    }
}

#[cfg(test)]
impl ConfirmScene {
    pub(crate) fn resource(self, role: Option<Role>) -> Resource {
        Resource::ActAsConfirm(Box::new(crate::model::ActAsConfirmView {
            target: merge_request(project_view(self.project, role), self.own),
            intent: if self.unapprove {
                Intent::Unapprove
            } else {
                Intent::Approve
            },
            integration_enabled: self.enabled,
            bound: self.bound,
        }))
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

use Expect::Allow;

pub(crate) const fn via(person: Option<Role>, token: ViaToken, assertion: Assert) -> W {
    W::ActAs(Via {
        person,
        token,
        assertion,
        enabled: true,
    })
}

const DEV: Option<Role> = Some(Role::Developer);
const REPORTER: Option<Role> = Some(Role::Reporter);
const MAINTAINER: Option<Role> = Some(Role::Maintainer);

/// A Developer person approving with kanflow's token and the matching
/// assertion.
const APPROVER: W = via(DEV, ViaToken::Approver, Assert::Approve);
const UNAPPROVER: W = via(DEV, ViaToken::Approver, Assert::Unapprove);
/// A Developer person reading (no assertion).
const READER: W = via(DEV, ViaToken::Reader, Assert::None);

const fn scene(via_role: Option<Role>, bound: bool, target: Target, drift: Drift) -> F {
    F::ActAs(Scene {
        via_role,
        bound,
        project: P::Private,
        target,
        drift,
    })
}

const MR_OTHER: Target = Target::Mr { own: false };
/// Another's merge request of a bound project (the service account a
/// Reporter, as a binding makes it).
pub(crate) const BOUND_MR: F = scene(REPORTER, true, MR_OTHER, Drift::None);
const BOUND_PROJECT: F = scene(REPORTER, true, Target::Project, Drift::None);

#[rustfmt::skip]
pub(crate) const ACT_AS_ROWS: &[Row] = &[
    // 2. allowed: the reads, capped at the smaller role (both Reporter+).
    row(A::ProjectRead, READER, BOUND_PROJECT, Allow),
    row(A::MergeRequestList, READER, BOUND_PROJECT, Allow),
    row(A::MergeRequestRead, READER, BOUND_MR, Allow),
    row(A::ProjectRead, via(MAINTAINER, ViaToken::Reader, Assert::None), BOUND_PROJECT, Allow),
    row(A::ProjectRead, via(REPORTER, ViaToken::Approver, Assert::None), BOUND_PROJECT, Allow),
    // Approve / unapprove with the matching assertion.
    row(A::MergeRequestApprove, APPROVER, BOUND_MR, Allow),
    row(A::MergeRequestApprove, via(MAINTAINER, ViaToken::Approver, Assert::Approve), BOUND_MR, Allow),
    row(A::MergeRequestUnapprove, UNAPPROVER, BOUND_MR, Allow),
    // 3. denied changes.
    row(A::MergeRequestApprove, via(DEV, ViaToken::Approver, Assert::None), BOUND_MR, deny(E::ActAsAssertionInvalid)),
    row(A::MergeRequestApprove, UNAPPROVER, BOUND_MR, deny(E::ActAsAssertionInvalid)),
    row(A::MergeRequestUnapprove, APPROVER, BOUND_MR, deny(E::ActAsAssertionInvalid)),
    row(A::MergeRequestUnapprove, via(DEV, ViaToken::Approver, Assert::None), BOUND_MR, deny(E::ActAsAssertionInvalid)),
    row(A::MergeRequestApprove, via(DEV, ViaToken::Approver, Assert::ApproveOtherMr), BOUND_MR, deny(E::ActAsAssertionInvalid)),
    row(A::MergeRequestApprove, APPROVER, scene(REPORTER, true, MR_OTHER, Drift::BodySha), deny(E::ActAsAssertionInvalid)),
    row(A::MergeRequestApprove, APPROVER, scene(REPORTER, true, MR_OTHER, Drift::Head), deny(E::ActAsAssertionInvalid)),
    row(A::MergeRequestApprove, APPROVER, scene(REPORTER, true, MR_OTHER, Drift::Retargeted), deny(E::ActAsAssertionInvalid)),
    row(A::MergeRequestApprove, APPROVER, scene(REPORTER, true, MR_OTHER, Drift::TargetTip), deny(E::ActAsAssertionInvalid)),
    row(A::MergeRequestUnapprove, UNAPPROVER, scene(REPORTER, true, MR_OTHER, Drift::Head), deny(E::ActAsAssertionInvalid)),
    row(A::MergeRequestApprove, APPROVER, scene(REPORTER, true, Target::Mr { own: true }, Drift::None), deny(E::SelfApprovalRefused)),
    row(A::MergeRequestApprove, via(REPORTER, ViaToken::Approver, Assert::Approve), BOUND_MR, deny(E::Forbidden)),
    row(A::MergeRequestApprove, via(DEV, ViaToken::Reader, Assert::Approve), BOUND_MR, deny(E::InsufficientScope)),
    row(A::MergeRequestApprove, via(DEV, ViaToken::NoActAs, Assert::Approve), BOUND_MR, deny(E::InsufficientScope)),
    row(A::MergeRequestApprove, APPROVER, scene(REPORTER, false, MR_OTHER, Drift::None), deny(E::NotFound)),
    row(A::MergeRequestApprove, W::ActAs(Via { person: DEV, token: ViaToken::Approver, assertion: Assert::Approve, enabled: false }), BOUND_MR, deny(E::Forbidden)),
    // An administrator without a membership: no administrator right passes.
    row(A::MergeRequestApprove, via(None, ViaToken::Approver, Assert::Approve), BOUND_MR, deny(E::NotFound)),
    row(A::MergeRequestApprove, via(DEV, ViaToken::Agent, Assert::Approve), BOUND_MR, deny(E::Forbidden)),
    row(A::MergeRequestUnapprove, via(DEV, ViaToken::Agent, Assert::Unapprove), BOUND_MR, deny(E::Forbidden)),
    // 4. denied reads.
    row(A::ProjectRead, via(None, ViaToken::Reader, Assert::None), BOUND_PROJECT, deny(E::NotFound)),
    row(A::MergeRequestRead, via(None, ViaToken::Reader, Assert::None), BOUND_MR, deny(E::NotFound)),
    row(A::ProjectRead, READER, scene(None, true, Target::Project, Drift::None), deny(E::NotFound)),
    row(A::ProjectRead, READER, scene(REPORTER, false, Target::Project, Drift::None), deny(E::NotFound)),
    row(A::MergeRequestList, READER, scene(REPORTER, false, Target::Project, Drift::None), deny(E::NotFound)),
    row(A::ProjectRead, via(MAINTAINER, ViaToken::Reader, Assert::None), scene(Some(Role::Guest), true, Target::Project, Drift::None), deny(E::Forbidden)),
    row(A::MergeRequestRead, via(MAINTAINER, ViaToken::Reader, Assert::None), scene(Some(Role::Guest), true, MR_OTHER, Drift::None), deny(E::Forbidden)),
    row(A::ProjectRead, via(Some(Role::Guest), ViaToken::Reader, Assert::None), BOUND_PROJECT, deny(E::Forbidden)),
    row(A::ProjectRead, via(DEV, ViaToken::Reader, Assert::Approve), BOUND_PROJECT, deny(E::ActAsAssertionInvalid)),
    row(A::MergeRequestRead, APPROVER, BOUND_MR, deny(E::ActAsAssertionInvalid)),
    row(A::ProjectRead, READER, scene(REPORTER, true, Target::Absent, Drift::None), deny(E::NotFound)),
    row(A::ProjectRead, via(DEV, ViaToken::NoActAs, Assert::None), BOUND_PROJECT, deny(E::InsufficientScope)),
    row(A::ProjectRead, via(DEV, ViaToken::Agent, Assert::None), BOUND_PROJECT, deny(E::Forbidden)),
    row(A::ProjectRead, W::ActAs(Via { person: DEV, token: ViaToken::Reader, assertion: Assert::None, enabled: false }), BOUND_PROJECT, deny(E::Forbidden)),
    // Review (authz HIGH): a route whose loader adds no act-as facts
    // (approval rules, merge settings, integration bindings) is refused:
    // the plain resource is never decided as the person's.
    row(A::ProjectRead, READER, F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeRequestList, READER, F::PrivateProject, deny(E::Forbidden)),
    row(A::MergeRequestRead, READER, F::Mr(crate::matrix_review::MrFixture::other()), deny(E::Forbidden)),
    // 5. the claim: the integration's own service account, `act_as`.
    row(A::ActAsAssertionClaim, W::ServiceAccountTokenActAs, F::IntegrationFeed { own: true, enabled: true, act_as: false }, Allow),
    row(A::ActAsAssertionClaim, W::ServiceAccountTokenActAs, F::IntegrationFeed { own: false, enabled: true, act_as: false }, deny(E::NotFound)),
    row(A::ActAsAssertionClaim, W::ServiceAccountTokenActAs, F::IntegrationFeed { own: true, enabled: false, act_as: false }, deny(E::Forbidden)),
    row(A::ActAsAssertionClaim, W::ServiceAccountAgentTokenActAs, F::IntegrationFeed { own: true, enabled: true, act_as: false }, deny(E::Forbidden)),
    row(A::ActAsAssertionClaim, W::ServiceAccountTokenReadApi, F::IntegrationFeed { own: true, enabled: true, act_as: false }, deny(E::InsufficientScope)),
    row(A::ActAsAssertionClaim, W::Admin, F::IntegrationFeed { own: true, enabled: true, act_as: false }, deny(E::NotFound)),
    row(A::ActAsAssertionClaim, W::Developer, F::IntegrationFeed { own: true, enabled: true, act_as: false }, deny(E::NotFound)),
    row(A::ActAsAssertionClaim, APPROVER, F::IntegrationFeed { own: true, enabled: true, act_as: false }, deny(E::Forbidden)),
];

const fn confirm(unapprove: bool, enabled: bool, bound: bool, own: bool) -> F {
    F::Confirm(ConfirmScene {
        unapprove,
        enabled,
        bound,
        own,
        project: P::Private,
    })
}

const APPROVE_OK: F = confirm(false, true, true, false);

#[rustfmt::skip]
pub(crate) const CONFIRM_ROWS: &[Row] = &[
    row(A::ActAsConfirmView, W::Developer, APPROVE_OK, Allow),
    row(A::ActAsAssertionIssue, W::Developer, APPROVE_OK, Allow),
    row(A::ActAsAssertionIssue, W::Developer, confirm(true, true, true, false), Allow),
    row(A::ActAsConfirmView, W::Developer, confirm(true, true, true, false), Allow),
    // A token, an act-as request, an agent: never.
    row(A::ActAsConfirmView, W::DevTokenMergeApprove, APPROVE_OK, deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::DevTokenMergeApprove, APPROVE_OK, deny(E::NotFound)),
    row(A::ActAsAssertionIssue, APPROVER, APPROVE_OK, deny(E::NotFound)),
    row(A::ActAsConfirmView, APPROVER, APPROVE_OK, deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::ServiceAccountAdminSession, APPROVE_OK, deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::BotAdminSession, APPROVE_OK, deny(E::NotFound)),
    row(A::ActAsConfirmView, W::ServiceAccountAdminSession, APPROVE_OK, deny(E::NotFound)),
    row(A::ActAsConfirmView, W::BotAdminSession, APPROVE_OK, deny(E::NotFound)),
    // Not bound, disabled, the intent's action refused, invisible.
    row(A::ActAsAssertionIssue, W::Developer, confirm(false, true, false, false), deny(E::NotFound)),
    row(A::ActAsConfirmView, W::Developer, confirm(false, true, false, false), deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::Developer, confirm(false, false, true, false), deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::Developer, confirm(false, true, true, true), deny(E::NotFound)),
    row(A::ActAsConfirmView, W::Developer, confirm(false, true, true, true), deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::Reporter, APPROVE_OK, deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::NonMember, APPROVE_OK, deny(E::NotFound)),
    row(A::ActAsConfirmView, W::NonMember, APPROVE_OK, deny(E::NotFound)),
    // An administrator's session sees every merge request and may approve
    // it (the act-as use later decides without administrator rights).
    row(A::ActAsAssertionIssue, W::Admin, APPROVE_OK, Allow),
    row(A::ActAsConfirmView, W::Admin, APPROVE_OK, Allow),
];

/// Rows 1: every action an act-as request never performs, for a Developer
/// person with a valid approve assertion on a bound project
/// (`act_as_denied_for_<action>`, ADR-0024 #38). The act-as
/// confirmation answers every refusal with the invisible project's 404
/// (ADR-0025, decision of 2026-11-10).
pub(crate) fn generated_denies() -> Vec<Row> {
    A::ALL
        .iter()
        .filter(|a| a.act_as_rule() == ActAsRule::Denied)
        .map(|&a| {
            let code = if matches!(a, A::ActAsConfirmView | A::ActAsAssertionIssue) {
                E::NotFound
            } else {
                E::Forbidden
            };
            row(a, APPROVER, BOUND_MR, deny(code))
        })
        .collect()
}
