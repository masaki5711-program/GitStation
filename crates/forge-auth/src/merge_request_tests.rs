//! `authorize()` on merge requests (M3 R1a, ADR-0024 §12): the denial
//! reasons the matrix rows cannot show, the 404 of an invisible project,
//! and the proof's id.

use forge_core::{MergeRequestState, ProjectId, RefName, UserId};
use forge_errors::{ErrorCode, ForgeError};

use crate::matrix::ME;
use crate::{
    authorize, prove, Action, Credential, LoadedMergeRequest, LoadedProject, MergeRequestCtx,
    MergeRequestEdit, Principal, ProjectView, Resource, ResourceCtx, Role, Visibility,
};

fn branch(raw: &str) -> RefName {
    RefName::parse(raw).expect("ref")
}

fn session() -> Principal {
    Principal::authenticated(ME, false, true, Credential::Session)
}

fn mr(role: Option<Role>, author: UserId, state: MergeRequestState) -> Resource {
    built(
        role,
        author,
        state,
        ("refs/heads/feature", "refs/heads/main"),
        MergeRequestEdit::default(),
    )
}

fn built(
    role: Option<Role>,
    author: UserId,
    state: MergeRequestState,
    (source, target): (&str, &str),
    edit: MergeRequestEdit,
) -> Resource {
    Resource::MergeRequest {
        project: ProjectView {
            visibility: Visibility::Private,
            role,
            pending_deletion: false,
        },
        author,
        state,
        draft: false,
        source_branch: branch(source),
        target_branch: branch(target),
        edit,
    }
}

fn reason(err: &ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.to_string())
        .unwrap_or_default()
}

fn denied(action: Action, resource: &Resource) -> (ErrorCode, String) {
    let err = authorize(&session(), action, resource).expect_err("denied");
    (err.code(), reason(&err))
}

#[test]
fn merge_request_branch_refusals_name_their_reason_in_every_arm() {
    let other = UserId::new(2);
    let with = |source: &str, target: &str| {
        built(
            Some(Role::Owner),
            ME,
            MergeRequestState::Open,
            (source, target),
            MergeRequestEdit::default(),
        )
    };
    for action in [
        Action::MergeRequestCreate,
        Action::MergeRequestRead,
        Action::MergeRequestUpdate,
        Action::MergeRequestClose,
        Action::MergeRequestReopen,
    ] {
        assert_eq!(
            denied(action, &with("refs/heads/main", "refs/heads/main")),
            (ErrorCode::Forbidden, "source_equals_target".to_owned()),
            "{action:?}"
        );
        assert_eq!(
            denied(action, &with("refs/heads/a", "refs/tags/v1")),
            (ErrorCode::Forbidden, "not_a_branch".to_owned()),
            "{action:?}"
        );
        assert_eq!(
            denied(
                action,
                &with("refs/merge-requests/1/head", "refs/heads/main")
            ),
            (ErrorCode::Forbidden, "not_a_branch".to_owned()),
            "{action:?}"
        );
    }
    // A create is for oneself.
    assert_eq!(
        denied(
            Action::MergeRequestCreate,
            &mr(Some(Role::Owner), other, MergeRequestState::Open)
        ),
        (ErrorCode::Forbidden, "foreign_author".to_owned())
    );
}

#[test]
fn merge_request_state_and_draft_refusals_name_their_reason() {
    let other = UserId::new(2);
    let merged = mr(Some(Role::Owner), other, MergeRequestState::Merged);
    for action in [Action::MergeRequestClose, Action::MergeRequestReopen] {
        assert_eq!(
            denied(action, &merged),
            (ErrorCode::Forbidden, "merged".to_owned()),
            "{action:?}"
        );
    }
    let edit = |role, state, set_draft: Option<bool>, target: Option<&str>| {
        built(
            Some(role),
            other,
            state,
            ("refs/heads/feature", "refs/heads/main"),
            MergeRequestEdit {
                target_branch: target.map(branch),
                draft: set_draft,
            },
        )
    };
    assert_eq!(
        denied(
            Action::MergeRequestUpdate,
            &edit(Role::Developer, MergeRequestState::Open, Some(true), None)
        ),
        (ErrorCode::Forbidden, "foreign_draft".to_owned())
    );
    assert_eq!(
        denied(
            Action::MergeRequestUpdate,
            &edit(
                Role::Owner,
                MergeRequestState::Closed,
                None,
                Some("refs/heads/release")
            )
        ),
        (ErrorCode::Forbidden, "not_open".to_owned())
    );
    // The same target is no change: allowed on a closed one.
    authorize(
        &session(),
        Action::MergeRequestUpdate,
        &edit(
            Role::Developer,
            MergeRequestState::Closed,
            None,
            Some("refs/heads/main"),
        ),
    )
    .expect("same target");
}

/// ADR-0024 §12: an invisible project's merge request is the same 404
/// (`FG-REPO-0005`) for every merge request action, administrators' agents
/// and all; a missing iid reaches `authorize()` as `Unresolved` and is the
/// same code (the route test pins the identical body).
#[test]
fn invisible_merge_request_is_indistinguishable_from_missing() {
    let invisible = mr(None, UserId::new(2), MergeRequestState::Open);
    for action in [
        Action::MergeRequestCreate,
        Action::MergeRequestRead,
        Action::MergeRequestUpdate,
        Action::MergeRequestClose,
        Action::MergeRequestReopen,
    ] {
        assert_eq!(
            denied(action, &invisible),
            (ErrorCode::NotFound, "not_visible".to_owned()),
            "{action:?}"
        );
        let missing = authorize(
            &session(),
            action,
            &Resource::Unresolved(crate::UnresolvedReason::Absent),
        )
        .expect_err("absent");
        assert_eq!(missing.code(), ErrorCode::NotFound, "{action:?}");
    }
    let list = authorize(
        &session(),
        Action::MergeRequestList,
        &Resource::Project(ProjectView {
            visibility: Visibility::Private,
            role: None,
            pending_deletion: false,
        }),
    )
    .expect_err("invisible");
    assert_eq!(list.code(), ErrorCode::NotFound);
}

#[test]
fn a_merge_request_proof_carries_the_project_and_the_iid() {
    let project = LoadedProject {
        id: ProjectId::new(7),
        visibility: Visibility::Private,
        role: Some(Role::Developer),
        pending_deletion: false,
    };
    let loaded = LoadedMergeRequest {
        iid: Some(3),
        author: UserId::new(2),
        state: MergeRequestState::Open,
        draft: false,
        source_branch: branch("refs/heads/feature"),
        target_branch: branch("refs/heads/main"),
    };
    let ctx = MergeRequestCtx::new(project, loaded.clone()); // clone: compared below
    assert_eq!(ctx.id(), (ProjectId::new(7), Some(3)));
    let proof = prove(&session(), Action::MergeRequestClose, ctx).expect("developer closes");
    assert_eq!(proof.id(), &(ProjectId::new(7), Some(3)));
    assert_eq!(proof.ctx().merge_request(), &loaded);
    // A create has no iid yet.
    let new = MergeRequestCtx::new(
        project,
        LoadedMergeRequest {
            iid: None,
            author: ME,
            ..loaded
        },
    );
    let proof = prove(&session(), Action::MergeRequestCreate, new).expect("developer opens");
    assert_eq!(proof.id(), &(ProjectId::new(7), None));
}
