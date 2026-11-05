//! Acting for a person (M3 S2, ADR-0025 §2–§5): the allowlist's
//! invariants, the act-as arm's order and reasons, and the session twins
//! of the allowed rows.

use forge_core::act_as::Intent;
use forge_errors::ErrorCode;

use crate::matrix::{Actor, Expect, Fixture, ME, ROWS};
use crate::matrix_review::MrProject;
use crate::matrix_rows_act_as::{Assert, Drift, Scene, Target, Via, ViaToken};
use crate::{
    act_as_admission, authorize, AccountKind, ActAsRule, Action, Credential, Principal, Role, Scope,
};

fn reason(e: &forge_errors::ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

const fn via(person: Option<Role>, token: ViaToken, assertion: Assert) -> Via {
    Via {
        person,
        token,
        assertion,
        enabled: true,
    }
}

const fn scene(via_role: Option<Role>, bound: bool, target: Target, drift: Drift) -> Scene {
    Scene {
        via_role,
        bound,
        project: MrProject::Private,
        target,
        drift,
    }
}

const DEV: Option<Role> = Some(Role::Developer);
const REPORTER: Option<Role> = Some(Role::Reporter);
const MR: Target = Target::Mr { own: false };

/// Decide `action` for `v` on `s`; the error's code and reason.
fn decide(action: Action, v: Via, s: Scene) -> Result<(), (ErrorCode, String)> {
    authorize(&v.principal(), action, &s.resource(v.person)).map_err(|e| (e.code(), reason(&e)))
}

fn approver() -> Via {
    via(DEV, ViaToken::Approver, Assert::Approve)
}

#[test]
fn every_people_only_act_as_action_requires_an_assertion() {
    for &a in Action::ALL {
        if a.is_people_only() && a.act_as_rule() != ActAsRule::Denied {
            assert!(
                matches!(a.act_as_rule(), ActAsRule::RequiresAssertion(_)),
                "{a:?}"
            );
        }
    }
}

#[test]
fn no_session_only_action_is_act_as() {
    for &a in Action::ALL {
        if a.is_session_only() || a.is_public() || a.is_git_operation() {
            assert_eq!(a.act_as_rule(), ActAsRule::Denied, "{a:?}");
        }
    }
}

#[test]
fn every_unasserted_act_as_action_is_a_read() {
    for &a in Action::ALL {
        if a.act_as_rule() == ActAsRule::ReadWithoutAssertion {
            assert_eq!(a.required_scope(), Some(Scope::ReadApi), "{a:?}");
            assert!(!a.is_people_only(), "{a:?}");
        }
    }
    // The intents are one to one with the actions.
    let intents: Vec<Intent> = Action::ALL
        .iter()
        .filter_map(|a| match a.act_as_rule() {
            ActAsRule::RequiresAssertion(i) => Some(i),
            ActAsRule::ReadWithoutAssertion | ActAsRule::Denied => None,
        })
        .collect();
    assert_eq!(intents, vec![Intent::Approve, Intent::Unapprove]);
}

#[test]
fn act_as_merge_is_not_allowed() {
    for a in [
        Action::MergeRequestMerge,
        Action::MergeSettingsManage,
        Action::ApprovalRuleManage,
        Action::CodeownersEnforcementToggle,
        Action::ProtectedRefManage,
        Action::MergeRequestCreate,
        Action::DiscussionResolve,
        Action::WorkItemLink,
        Action::CheckWrite,
        Action::IntegrationEventRead,
        Action::IntegrationBindingManage,
    ] {
        assert_eq!(
            decide(a, approver(), scene(REPORTER, true, MR, Drift::None)),
            Err((ErrorCode::Forbidden, "act_as_not_allowed".into())),
            "{a:?}"
        );
    }
}

#[test]
fn act_as_approve_without_assertion_is_refused() {
    let err = authorize(
        &via(DEV, ViaToken::Approver, Assert::None).principal(),
        Action::MergeRequestApprove,
        &scene(REPORTER, true, MR, Drift::None).resource(DEV),
    )
    .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::ActAsAssertionInvalid);
    assert_eq!(err.code().id(), "FG-AUTH-0020");
    assert_eq!(reason(&err), "missing");
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "act_as_rule" && v == "requires_assertion"));
}

#[test]
fn assertion_on_a_read_is_refused() {
    for a in [
        Action::ProjectRead,
        Action::MergeRequestRead,
        Action::MergeRequestList,
    ] {
        let target = if a == Action::MergeRequestRead {
            MR
        } else {
            Target::Project
        };
        assert_eq!(
            decide(a, approver(), scene(REPORTER, true, target, Drift::None)),
            Err((ErrorCode::ActAsAssertionInvalid, "unexpected".into())),
            "{a:?}"
        );
    }
}

#[test]
fn act_as_read_is_capped_by_the_service_accounts_role() {
    let maintainer = via(Some(Role::Maintainer), ViaToken::Reader, Assert::None);
    // The person reads; the service account, a Guest, does not: refused.
    let err = authorize(
        &maintainer.principal(),
        Action::ProjectRead,
        &scene(Some(Role::Guest), true, Target::Project, Drift::None).resource(maintainer.person),
    )
    .expect_err("capped");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "effective_role" && v == "guest"));
    // The service account is no member: as invisible as to it.
    assert_eq!(
        decide(
            Action::ProjectRead,
            maintainer,
            scene(None, true, Target::Project, Drift::None)
        ),
        Err((ErrorCode::NotFound, "act_as_not_visible".into()))
    );
    // The person is no member (an administrator's flag is never carried).
    assert_eq!(
        decide(
            Action::ProjectRead,
            via(None, ViaToken::Reader, Assert::None),
            scene(REPORTER, true, Target::Project, Drift::None)
        ),
        Err((ErrorCode::NotFound, "act_as_not_visible".into()))
    );
    // Both Reporter or above: allowed.
    assert_eq!(
        decide(
            Action::ProjectRead,
            maintainer,
            scene(REPORTER, true, Target::Project, Drift::None)
        ),
        Ok(())
    );
}

#[test]
fn act_as_on_an_unbound_project_is_not_found() {
    for (a, v, t) in [
        (Action::MergeRequestApprove, approver(), MR),
        (
            Action::ProjectRead,
            via(DEV, ViaToken::Reader, Assert::None),
            Target::Project,
        ),
    ] {
        assert_eq!(
            decide(a, v, scene(REPORTER, false, t, Drift::None)),
            Err((ErrorCode::NotFound, "act_as_unbound".into())),
            "{a:?}"
        );
    }
}

#[test]
fn act_as_self_approval_is_refused() {
    assert_eq!(
        decide(
            Action::MergeRequestApprove,
            approver(),
            scene(REPORTER, true, Target::Mr { own: true }, Drift::None)
        ),
        Err((ErrorCode::SelfApprovalRefused, "self_approval".into()))
    );
}

#[test]
fn assertion_bound_to_head_and_target() {
    for (drift, why) in [
        (Drift::BodySha, "sha_mismatch"),
        (Drift::Head, "head_changed"),
        (Drift::TargetTip, "target_changed"),
    ] {
        assert_eq!(
            decide(
                Action::MergeRequestApprove,
                approver(),
                scene(REPORTER, true, MR, drift)
            ),
            Err((ErrorCode::ActAsAssertionInvalid, why.into())),
            "{drift:?}"
        );
    }
}

#[test]
fn assertion_bound_to_target_ref() {
    assert_eq!(
        decide(
            Action::MergeRequestApprove,
            approver(),
            scene(REPORTER, true, MR, Drift::Retargeted)
        ),
        Err((ErrorCode::ActAsAssertionInvalid, "target_retargeted".into()))
    );
}

#[test]
fn assertion_bound_to_action_resource_and_user() {
    let ok = scene(REPORTER, true, MR, Drift::None);
    assert_eq!(
        decide(
            Action::MergeRequestApprove,
            via(DEV, ViaToken::Approver, Assert::Unapprove),
            ok
        ),
        Err((ErrorCode::ActAsAssertionInvalid, "wrong_action".into()))
    );
    assert_eq!(
        decide(Action::MergeRequestUnapprove, approver(), ok),
        Err((ErrorCode::ActAsAssertionInvalid, "wrong_action".into()))
    );
    assert_eq!(
        decide(
            Action::MergeRequestApprove,
            via(DEV, ViaToken::Approver, Assert::ApproveOtherMr),
            ok
        ),
        Err((ErrorCode::ActAsAssertionInvalid, "wrong_resource".into()))
    );
    // The person and the integration are forge-db's verification
    // (`wrong_user`, `wrong_integration`): an assertion only reaches the
    // credential once they match.
}

#[test]
fn the_credential_is_decided_again_in_the_act_as_arm() {
    let ok = scene(REPORTER, true, MR, Drift::None);
    assert_eq!(
        decide(
            Action::MergeRequestApprove,
            via(DEV, ViaToken::Agent, Assert::Approve),
            ok
        ),
        Err((ErrorCode::Forbidden, "agent_token".into()))
    );
    let err = authorize(
        &via(DEV, ViaToken::NoActAs, Assert::Approve).principal(),
        Action::MergeRequestApprove,
        &ok.resource(DEV),
    )
    .expect_err("no act_as");
    assert_eq!(err.code(), ErrorCode::InsufficientScope);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "required_scope" && v == "act_as"));
    let disabled = Via {
        enabled: false,
        ..approver()
    };
    assert_eq!(
        decide(Action::MergeRequestApprove, disabled, ok),
        Err((ErrorCode::Forbidden, "integration_disabled".into()))
    );
    // A person's account kind is the resolution's to check; a principal
    // for another kind is refused all the same.
    let p = Principal::act_as(
        ME,
        true,
        AccountKind::ServiceAccount,
        approver().credential(),
    );
    let err = authorize(&p, Action::MergeRequestApprove, &ok.resource(DEV)).expect_err("kind");
    assert_eq!(reason(&err), "act_as_target_not_human");
    // A resource loaded without the act-as facts is never decided as the
    // person's.
    let plain = ok.person_resource(DEV);
    let err = authorize(&approver().principal(), Action::MergeRequestApprove, &plain)
        .expect_err("no facts");
    assert_eq!(reason(&err), "act_as_facts_missing");
    // No administrator right, whatever the person's account says.
    assert!(!approver().principal().admin_rights());
}

#[test]
fn act_as_admission_checks_the_token_before_any_lookup() {
    let me = |kind, credential| Principal::authenticated(ME, false, true, credential, kind);
    let full = Credential::Token(vec![Scope::ReadApi, Scope::MergeApprove, Scope::ActAs]);
    assert!(act_as_admission(&me(AccountKind::ServiceAccount, full.clone())).is_ok());
    for p in [
        me(AccountKind::Human, Credential::Session),
        me(AccountKind::Human, full.clone()),
        me(AccountKind::Bot, full.clone()),
        Principal::authenticated(
            ME,
            true,
            true,
            Credential::Session,
            AccountKind::ServiceAccount,
        ),
    ] {
        let err = act_as_admission(&p).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::Forbidden);
        assert_eq!(reason(&err), "act_as_requires_service_account");
    }
    let err = act_as_admission(&me(
        AccountKind::ServiceAccount,
        Credential::Token(vec![Scope::Api, Scope::Admin, Scope::MergeApprove]),
    ))
    .expect_err("no act_as");
    assert_eq!(err.code(), ErrorCode::InsufficientScope);
    let agent = me(AccountKind::ServiceAccount, full).as_agent(true);
    assert_eq!(
        reason(&act_as_admission(&agent).expect_err("agent")),
        "agent_token"
    );
}

#[test]
fn act_as_allow_rows_have_an_allowed_session_twin() {
    let mut twins = 0;
    for r in ROWS.iter().filter(|r| r.expect == Expect::Allow) {
        let (Actor::ActAs(v), Fixture::ActAs(s)) = (r.actor, r.fixture) else {
            continue;
        };
        let session =
            Principal::authenticated(ME, false, true, Credential::Session, AccountKind::Human);
        assert!(
            authorize(&session, r.action, &s.person_resource(v.person)).is_ok(),
            "{:?} {v:?} {s:?}: the person's own session must be allowed",
            r.action
        );
        twins += 1;
    }
    assert!(twins >= 8, "{twins}");
}

#[test]
fn every_denied_action_has_a_generated_act_as_row() {
    for &a in Action::ALL {
        if a.act_as_rule() != ActAsRule::Denied {
            continue;
        }
        assert!(
            ROWS.iter().any(|r| r.action == a
                && matches!(r.actor, Actor::ActAs(_))
                && r.expect == Expect::Deny(ErrorCode::Forbidden)),
            "act_as_denied_for_{}",
            a.name()
        );
    }
}

#[test]
fn the_act_as_viewer_trials_approve_with_a_matching_assertion() {
    use crate::{LoadedMergeRequest, LoadedProject, MergeRequestCtx, ViaView};
    use forge_core::{MergeRequestId, MergeRequestState, ProjectId, RefName};
    let name = |r: &str| RefName::parse(r).expect("ref");
    let project = LoadedProject {
        id: ProjectId::new(2),
        visibility: crate::Visibility::Private,
        role: Some(Role::Developer),
        pending_deletion: false,
    };
    let mr = |author| LoadedMergeRequest {
        iid: Some(1),
        author,
        state: MergeRequestState::Open,
        draft: false,
        source_branch: name("refs/heads/feature"),
        target_branch: name("refs/heads/main"),
    };
    let subject = crate::ActAsSubject {
        project: ProjectId::new(2),
        merge_request: MergeRequestId::new(20),
        head_sha: Some("1".repeat(40)),
        target_ref: name("refs/heads/main"),
        target_sha: Some("2".repeat(40)),
        requested_sha: None,
    };
    let facts = ViaView {
        role: Some(Role::Reporter),
        bound: true,
    };
    let reader = via_reader();
    let other = MergeRequestCtx::new(project, mr(crate::matrix::OTHER))
        .acting(facts, Some(subject.clone()));
    let v = other.act_as_viewer(&reader.principal()).expect("act-as");
    assert!(v.requires_assertion);
    // The reader's token lacks merge_approve: refused with its scope.
    assert!(!v.can_approve.allowed);
    let approver = via(DEV, ViaToken::Approver, Assert::None);
    let v = other.act_as_viewer(&approver.principal()).expect("act-as");
    assert!(v.can_approve.allowed, "{v:?}");
    assert!(v.can_unapprove.allowed);
    // The author: never.
    let own = MergeRequestCtx::new(project, mr(ME)).acting(facts, Some(subject));
    let v = own.act_as_viewer(&approver.principal()).expect("act-as");
    assert_eq!(v.can_approve.reason.as_deref(), Some("self_approval"));
    // Not acting for a person: no act-as viewer.
    let session =
        Principal::authenticated(ME, false, true, Credential::Session, AccountKind::Human);
    assert!(other.act_as_viewer(&session).is_none());
}

fn via_reader() -> Via {
    via(DEV, ViaToken::Reader, Assert::None)
}
