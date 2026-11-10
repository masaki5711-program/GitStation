//! The permission matrix rows of the integration binding routes of M3 X3
//! (plan 03 §7.1, ADR-0016 Amendment X3, ADR-0025 Amendment X3):
//!
//! 1. acting for a person on a binding (`IntegrationBindingManage` with a
//!    `binding_set` assertion, `IntegrationBindingRemove` with a
//!    `binding_remove` one): allowed for a Maintainer person with the
//!    matching assertion on the request's own integration, whether or not
//!    the project is bound yet; denied without an assertion, with the wrong
//!    intent, another kanflow project, another setting, another
//!    integration in the path, a Developer, an invisible or absent project,
//!    without `act_as` or `api`, an agent's token, a disabled integration,
//!    a pending deletion, a merge request assertion, and on a route whose
//!    loader adds no binding facts (the project routes);
//! 2. the binding confirmation (`ActAsAssertionIssue` with a binding
//!    intent): the person's web session, Maintainer;
//! 3. `RepositorySearch`: the route for any active caller with `read_api`
//!    (and a person acted for, without an assertion), each candidate only
//!    for a Maintainer — the person's own role when acted for;
//! 4. `IntegrationBindingList`: the integration's own service account,
//!    never acted for.
//!
//! The service account / bot twin rows of `IntegrationBindingRemove`
//! (people only) are in `matrix_rows_people.rs`.

use forge_errors::ErrorCode as E;

use crate::action::Action as A;
use crate::matrix::{Actor as W, Expect, Fixture as F, Row};
use crate::matrix_review::MrProject as P;
use crate::matrix_rows_act_as::{via, Assert, Via, ViaToken, BOUND_MR};
use crate::model::Role;

#[cfg(test)]
use crate::model::{
    ActAsConfirmView, ActAsView, BindingSubject, Resource, UnresolvedReason, ViaView,
};
#[cfg(test)]
use forge_core::{act_as::Intent, IntegrationId, ProjectId};

/// What an act-as binding row writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Writes {
    /// `binding_set` of `P1`, shared.
    SetShared,
    /// `binding_remove` of `P1`.
    Remove,
}

/// The binding an act-as row acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BindingScene {
    pub project: P,
    /// Nothing at the path.
    pub absent: bool,
    /// The path's integration is the request's own (5); else another (6).
    pub own_integration: bool,
    pub writes: Writes,
}

/// The confirmation of a binding intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BindingConfirmScene {
    pub remove: bool,
    pub enabled: bool,
    /// The integration is bound to the project (for that kanflow project).
    pub bound: bool,
    pub project: P,
}

#[cfg(test)]
const PROJECT: ProjectId = ProjectId::new(2);

#[cfg(test)]
impl BindingScene {
    pub(crate) fn resource(self, role: Option<Role>) -> Resource {
        if self.absent {
            return Resource::Unresolved(UnresolvedReason::Absent);
        }
        let resolves = match self.writes {
            Writes::SetShared => Some(false),
            Writes::Remove => None,
        };
        Resource::ActAs(Box::new(ActAsView {
            inner: self.person_resource(role),
            via: ViaView {
                role: None,
                bound: false,
            },
            subject: None,
            binding: Some(BindingSubject {
                integration: IntegrationId::new(if self.own_integration { 5 } else { 6 }),
                project: PROJECT,
                external_project_id: "P1".to_owned(),
                resolves_work_items: resolves,
            }),
        }))
    }

    /// The person's own view (the session twin decides on it).
    pub(crate) fn person_resource(self, role: Option<Role>) -> Resource {
        Resource::Project(crate::matrix_rows_act_as::project_view(self.project, role))
    }
}

#[cfg(test)]
impl BindingConfirmScene {
    pub(crate) fn resource(self, role: Option<Role>) -> Resource {
        Resource::ActAsConfirm(Box::new(ActAsConfirmView {
            target: Resource::Project(crate::matrix_rows_act_as::project_view(self.project, role)),
            intent: if self.remove {
                Intent::BindingRemove
            } else {
                Intent::BindingSet
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

const MAINTAINER: Option<Role> = Some(Role::Maintainer);
const OWNER: Option<Role> = Some(Role::Owner);
const DEV: Option<Role> = Some(Role::Developer);

const fn binding(project: P, own_integration: bool, writes: Writes) -> F {
    F::ActAsBinding(BindingScene {
        project,
        absent: false,
        own_integration,
        writes,
    })
}

/// A shared bind of `P1` on a private project (not bound yet, or bound:
/// the binding intents do not look at `via_bound`).
const SET: F = binding(P::Private, true, Writes::SetShared);
const REMOVE: F = binding(P::Private, true, Writes::Remove);
/// A Maintainer person with kanflow's binding token.
const BINDER: W = via(MAINTAINER, ViaToken::Binder, Assert::BindShared);
const UNBINDER: W = via(MAINTAINER, ViaToken::Binder, Assert::Unbind);

#[rustfmt::skip]
pub(crate) const BINDING_ROWS: &[Row] = &[
    // 1. allowed: the matching assertion, the person's own Maintainer+.
    row(A::IntegrationBindingManage, BINDER, SET, Allow),
    row(A::IntegrationBindingManage, via(OWNER, ViaToken::Binder, Assert::BindShared), SET, Allow),
    row(A::IntegrationBindingManage, BINDER, binding(P::Internal, true, Writes::SetShared), Allow),
    row(A::IntegrationBindingRemove, UNBINDER, REMOVE, Allow),
    // Denied: no assertion, another intent, kanflow project or setting.
    row(A::IntegrationBindingManage, via(MAINTAINER, ViaToken::Binder, Assert::None), SET, deny(E::ActAsAssertionInvalid)),
    row(A::IntegrationBindingRemove, via(MAINTAINER, ViaToken::Binder, Assert::None), REMOVE, deny(E::ActAsAssertionInvalid)),
    row(A::IntegrationBindingManage, UNBINDER, SET, deny(E::ActAsAssertionInvalid)),
    row(A::IntegrationBindingRemove, BINDER, REMOVE, deny(E::ActAsAssertionInvalid)),
    row(A::IntegrationBindingManage, via(MAINTAINER, ViaToken::Binder, Assert::BindOtherKanflow), SET, deny(E::ActAsAssertionInvalid)),
    row(A::IntegrationBindingManage, via(MAINTAINER, ViaToken::Binder, Assert::BindResolving), SET, deny(E::ActAsAssertionInvalid)),
    row(A::IntegrationBindingManage, via(MAINTAINER, ViaToken::Binder, Assert::Approve), SET, deny(E::ActAsAssertionInvalid)),
    // A binding assertion never approves a merge request.
    row(A::MergeRequestApprove, via(DEV, ViaToken::Approver, Assert::BindShared), BOUND_MR, deny(E::ActAsAssertionInvalid)),
    // Another integration's binding route: not found.
    row(A::IntegrationBindingManage, BINDER, binding(P::Private, false, Writes::SetShared), deny(E::NotFound)),
    row(A::IntegrationBindingRemove, UNBINDER, binding(P::Private, false, Writes::Remove), deny(E::NotFound)),
    // The person's own role decides: a Developer, an invisible project, a
    // non-member administrator (no administrator right passes).
    row(A::IntegrationBindingManage, via(DEV, ViaToken::Binder, Assert::BindShared), SET, deny(E::Forbidden)),
    row(A::IntegrationBindingRemove, via(DEV, ViaToken::Binder, Assert::Unbind), REMOVE, deny(E::Forbidden)),
    row(A::IntegrationBindingManage, via(None, ViaToken::Binder, Assert::BindShared), SET, deny(E::NotFound)),
    row(A::IntegrationBindingManage, via(None, ViaToken::Binder, Assert::BindShared), binding(P::Internal, true, Writes::SetShared), deny(E::Forbidden)),
    row(A::IntegrationBindingManage, BINDER, binding(P::Deleted, true, Writes::SetShared), deny(E::PendingDeletion)),
    row(A::IntegrationBindingManage, BINDER, F::ActAsBinding(BindingScene { project: P::Private, absent: true, own_integration: true, writes: Writes::SetShared }), deny(E::NotFound)),
    // The token: `act_as`, `api`, not an agent's; an enabled integration.
    row(A::IntegrationBindingManage, via(MAINTAINER, ViaToken::NoActAs, Assert::BindShared), SET, deny(E::InsufficientScope)),
    row(A::IntegrationBindingManage, via(MAINTAINER, ViaToken::Reader, Assert::BindShared), SET, deny(E::InsufficientScope)),
    row(A::IntegrationBindingManage, via(MAINTAINER, ViaToken::Agent, Assert::BindShared), SET, deny(E::Forbidden)),
    row(A::IntegrationBindingManage, W::ActAs(Via { person: MAINTAINER, token: ViaToken::Binder, assertion: Assert::BindShared, enabled: false }), SET, deny(E::Forbidden)),
    // A route whose loader adds no binding facts (the project routes):
    // never decided as the person's.
    row(A::IntegrationBindingManage, BINDER, F::PrivateProject, deny(E::Forbidden)),
    row(A::IntegrationBindingRemove, UNBINDER, F::PrivateProject, deny(E::Forbidden)),
    // The integration's service account on its own never binds: the
    // people-only twins (`matrix_rows_people.rs`).
    // People's own removal (as IntegrationBindingManage).
    row(A::IntegrationBindingRemove, W::Maintainer, F::PrivateProject, Allow),
    row(A::IntegrationBindingRemove, W::MaintainerTokenApi, F::PrivateProject, Allow),
    row(A::IntegrationBindingRemove, W::Admin, F::PrivateProject, Allow),
    row(A::IntegrationBindingRemove, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::IntegrationBindingRemove, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::IntegrationBindingRemove, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::IntegrationBindingRemove, W::MaintainerTokenReadApi, F::PrivateProject, deny(E::InsufficientScope)),
    row(A::IntegrationBindingRemove, W::AgentMaintainerTokenApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::IntegrationBindingRemove, W::Disabled, F::PrivateProject, deny(E::AccountDisabled)),
    row(A::IntegrationBindingRemove, W::Anonymous, F::PrivateProject, deny(E::Unauthenticated)),
    // 2. the binding confirmation: the person's web session, Maintainer;
    // `binding_set` on a project not bound yet, `binding_remove` on a
    // bound one.
    row(A::ActAsAssertionIssue, W::Maintainer, confirm(false, true, false), Allow),
    row(A::ActAsAssertionIssue, W::Maintainer, confirm(true, true, true), Allow),
    row(A::ActAsAssertionIssue, W::Admin, confirm(false, true, false), Allow),
    row(A::ActAsAssertionIssue, W::Maintainer, confirm(true, true, false), deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::Maintainer, confirm(false, false, false), deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::Developer, confirm(false, true, false), deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::NonMember, confirm(false, true, false), deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::MaintainerTokenApi, confirm(false, true, false), deny(E::NotFound)),
    row(A::ActAsAssertionIssue, BINDER, confirm(false, true, false), deny(E::NotFound)),
    row(A::ActAsAssertionIssue, W::ServiceAccountAdminSession, confirm(false, true, false), deny(E::NotFound)),
    // 3. what the binding confirmation screen shows (`GET …/integration-bindings/act-as-confirmation`, ADR-0025
    // Amendment X3 §4): decided exactly like the press — whoever may not press sees nothing.
    row(A::ActAsConfirmView, W::Maintainer, confirm(false, true, false), Allow),
    row(A::ActAsConfirmView, W::Maintainer, confirm(true, true, true), Allow),
    row(A::ActAsConfirmView, W::Admin, confirm(false, true, false), Allow),
    row(A::ActAsConfirmView, W::Maintainer, confirm(true, true, false), deny(E::NotFound)),
    row(A::ActAsConfirmView, W::Maintainer, confirm(false, false, false), deny(E::NotFound)),
    row(A::ActAsConfirmView, W::Developer, confirm(false, true, false), deny(E::NotFound)),
    row(A::ActAsConfirmView, W::NonMember, confirm(false, true, false), deny(E::NotFound)),
    row(A::ActAsConfirmView, W::MaintainerTokenApi, confirm(false, true, false), deny(E::NotFound)),
    row(A::ActAsConfirmView, BINDER, confirm(false, true, false), deny(E::NotFound)),
    row(A::ActAsConfirmView, W::ServiceAccountAdminSession, confirm(false, true, false), deny(E::NotFound)),
];

const fn confirm(remove: bool, enabled: bool, bound: bool) -> F {
    F::BindingConfirm(BindingConfirmScene {
        remove,
        enabled,
        bound,
        project: P::Private,
    })
}

/// A person read acting for, without an assertion.
const SEARCHER: W = via(MAINTAINER, ViaToken::Reader, Assert::None);

#[rustfmt::skip]
pub(crate) const SEARCH_ROWS: &[Row] = &[
    // 3. the route: any active caller with `read_api`.
    row(A::RepositorySearch, W::Developer, F::Instance, Allow),
    row(A::RepositorySearch, W::DevTokenApi, F::Instance, Allow),
    row(A::RepositorySearch, W::MaintainerTokenReadApi, F::Instance, Allow),
    row(A::RepositorySearch, SEARCHER, F::Instance, Allow),
    row(A::RepositorySearch, W::DevTokenMergeApprove, F::Instance, deny(E::InsufficientScope)),
    row(A::RepositorySearch, W::Disabled, F::Instance, deny(E::AccountDisabled)),
    row(A::RepositorySearch, W::Anonymous, F::Instance, deny(E::Unauthenticated)),
    row(A::RepositorySearch, via(MAINTAINER, ViaToken::Reader, Assert::BindShared), F::Instance, deny(E::ActAsAssertionInvalid)),
    row(A::RepositorySearch, via(MAINTAINER, ViaToken::NoActAs, Assert::None), F::Instance, deny(E::InsufficientScope)),
    row(A::RepositorySearch, via(MAINTAINER, ViaToken::Agent, Assert::None), F::Instance, deny(E::Forbidden)),
    // Review LOW (b): never a service account, a bot or an agent alone.
    row(A::RepositorySearch, W::ServiceAccountTokenReadApi, F::Instance, deny(E::Forbidden)),
    row(A::RepositorySearch, W::BotTokenApi, F::Instance, deny(E::Forbidden)),
    row(A::RepositorySearch, W::AgentTokenReadApi, F::Instance, deny(E::Forbidden)),
    // Each candidate: what the caller (the person acted for) could bind.
    row(A::RepositorySearch, W::Maintainer, F::PrivateProject, Allow),
    row(A::RepositorySearch, W::Owner, F::PrivateProject, Allow),
    row(A::RepositorySearch, W::Admin, F::PrivateProject, Allow),
    row(A::RepositorySearch, SEARCHER, F::PrivateProject, Allow),
    row(A::RepositorySearch, W::Developer, F::PrivateProject, deny(E::Forbidden)),
    row(A::RepositorySearch, W::NonMember, F::InternalProject, deny(E::Forbidden)),
    row(A::RepositorySearch, W::NonMember, F::PrivateProject, deny(E::NotFound)),
    row(A::RepositorySearch, W::Maintainer, F::DeletedProject, deny(E::PendingDeletion)),
    row(A::RepositorySearch, W::ServiceAccountTokenReadApi, F::PrivateProject, deny(E::Forbidden)),
    row(A::RepositorySearch, via(DEV, ViaToken::Reader, Assert::None), F::PrivateProject, deny(E::Forbidden)),
    row(A::RepositorySearch, via(None, ViaToken::Reader, Assert::None), F::PrivateProject, deny(E::NotFound)),
    // A person acted for without a membership: no administrator right.
    row(A::RepositorySearch, via(None, ViaToken::Reader, Assert::None), F::InternalProject, deny(E::Forbidden)),
];

const OWN: F = F::IntegrationFeed {
    own: true,
    enabled: true,
    act_as: false,
};

#[rustfmt::skip]
pub(crate) const LIST_ROWS: &[Row] = &[
    // 4. the integration's own service account, `read_api`.
    row(A::IntegrationBindingList, W::ServiceAccountTokenReadApi, OWN, Allow),
    row(A::IntegrationBindingList, W::ServiceAccountTokenReadApi, F::IntegrationFeed { own: false, enabled: true, act_as: false }, deny(E::NotFound)),
    row(A::IntegrationBindingList, W::ServiceAccountTokenReadApi, F::IntegrationFeed { own: true, enabled: false, act_as: false }, deny(E::Forbidden)),
    row(A::IntegrationBindingList, W::ServiceAccountAgentTokenReadApi, OWN, deny(E::Forbidden)),
    row(A::IntegrationBindingList, W::ServiceAccountTokenActAs, OWN, deny(E::InsufficientScope)),
    row(A::IntegrationBindingList, W::Admin, OWN, deny(E::NotFound)),
    row(A::IntegrationBindingList, W::Developer, OWN, deny(E::NotFound)),
    row(A::IntegrationBindingList, SEARCHER, OWN, deny(E::Forbidden)),
    row(A::IntegrationBindingList, W::Anonymous, OWN, deny(E::Unauthenticated)),
];
