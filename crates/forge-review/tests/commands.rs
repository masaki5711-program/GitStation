#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The merge request commands against the development database (ADR-0011):
//! the proof the registry would hand over, decided again inside the
//! transaction on the locked facts; branches from a scripted probe.

mod common;

use common::*;
use forge_auth::{Action, MergeRequestEdit};
use forge_core::{EventType, MergeRequestState, Role};
use forge_db::projects;
use forge_db::test_support::{assert_reason, FreshDb};
use forge_errors::ErrorCode;
use forge_review::{EditFields, EditOutcome, NewFields, Review, StateOutcome};

fn fields(title: &str) -> NewFields {
    NewFields {
        title: title.to_owned(),
        description: "synthetic".to_owned(),
    }
}

#[tokio::test]
async fn a_developer_opens_and_the_author_edits_closes_and_reopens() {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let dev = user(&t, "dave").await;
    let web = project(&t, owner, &[(dev, Role::Developer)]).await;
    let p = web.project_id;
    let repo = ready(web.repository_id);
    let review = Review::new(
        t.db.clone(),
        Branches::with(&["main", "feature", "release"]),
    );
    let as_dev = session(dev);

    let proof = open_proof(&t, &as_dev, p, ("feature", "main"), true)
        .await
        .expect("a developer opens");
    let opened = review
        .open(&request("req-open"), &proof, repo, &fields("Add the form"))
        .await
        .expect("open");
    assert_eq!(
        (opened.iid, opened.author, opened.draft, opened.state),
        (1, dev, true, MergeRequestState::Open)
    );
    assert_eq!(opened.title, "Add the form");

    // The author marks it ready, retitles and retargets it in one edit.
    let edit = MergeRequestEdit {
        target_branch: Some(branch("release")),
        draft: Some(false),
    };
    let proof = stored_proof(&t, &review, &as_dev, Action::MergeRequestUpdate, p, 1, edit)
        .await
        .expect("the author edits");
    let edited = review
        .edit(
            &request("req-edit"),
            &proof,
            repo,
            &EditFields {
                title: Some("Add the login form".to_owned()),
                description: None,
            },
        )
        .await
        .expect("edit");
    assert_eq!(
        edited,
        EditOutcome::Changed(vec![
            EventType::MergeRequestUpdated,
            EventType::MergeRequestDraftChanged
        ])
    );
    let now = review
        .merge_request(p, 1)
        .await
        .expect("get")
        .expect("there");
    assert_eq!(
        (now.title.as_str(), now.draft, now.target_branch.as_str()),
        ("Add the login form", false, "refs/heads/release")
    );

    let proof = stored_proof(
        &t,
        &review,
        &as_dev,
        Action::MergeRequestClose,
        p,
        1,
        MergeRequestEdit::default(),
    )
    .await
    .expect("the author closes");
    assert_eq!(
        review
            .close(&request("req-close"), &proof)
            .await
            .expect("close"),
        StateOutcome::Changed
    );
    let proof = stored_proof(
        &t,
        &review,
        &as_dev,
        Action::MergeRequestReopen,
        p,
        1,
        MergeRequestEdit::default(),
    )
    .await
    .expect("the author reopens");
    assert_eq!(
        review
            .reopen(&request("req-reopen"), &proof, repo)
            .await
            .expect("reopen"),
        StateOutcome::Changed
    );
    let events = count(
        &t,
        "SELECT count(*) AS n FROM forge.events WHERE event_type LIKE 'merge_request.%'",
    )
    .await;
    assert_eq!(
        events, 5,
        "opened, updated, draft_changed, closed, reopened"
    );
    // The actor of every change is the caller.
    let actors = count(
        &t,
        &format!(
            "SELECT count(*) AS n FROM forge.audit_log WHERE action LIKE 'merge_request.%' \
             AND actor_kind = 'user' AND actor_user_id = {}",
            dev.get()
        ),
    )
    .await;
    assert_eq!(actors, 5);
    t.drop_now().await;
}

/// The decision binds the write: a role removed after the registry's proof
/// is seen inside the transaction (the project is no longer visible to the
/// caller: FG-REPO-0005), and nothing is written — no number drawn either.
#[tokio::test]
async fn a_role_removed_after_the_proof_is_denied_inside_the_transaction() {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let dev = user(&t, "dave").await;
    let web = project(&t, owner, &[(dev, Role::Developer)]).await;
    let p = web.project_id;
    let review = Review::new(t.db.clone(), Branches::with(&["main", "feature"]));
    let proof = open_proof(&t, &session(dev), p, ("feature", "main"), false)
        .await
        .expect("allowed before the change");
    projects::remove_member(&t.db, &system("req-remove"), p, dev, Role::Developer)
        .await
        .expect("remove");
    let err = review
        .open(
            &request("req-open"),
            &proof,
            ready(web.repository_id),
            &fields("Late"),
        )
        .await
        .expect_err("denied on the locked facts");
    assert_eq!(err.code(), ErrorCode::NotFound);
    let written = count(&t, "SELECT count(*) AS n FROM forge.merge_requests").await;
    assert_eq!(written, 0);
    let counters = count(&t, "SELECT count(*) AS n FROM forge.merge_request_counters").await;
    assert_eq!(counters, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn a_missing_branch_is_ref_not_found_and_writes_nothing() {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let web = project(&t, owner, &[]).await;
    let p = web.project_id;
    let repo = ready(web.repository_id);
    let branches = Branches::with(&["main", "feature"]);
    let review = Review::new(t.db.clone(), branches.clone());
    let as_owner = session(owner);
    let proof = open_proof(&t, &as_owner, p, ("nope", "main"), false)
        .await
        .expect("allowed");
    let err = review
        .open(&request("req-a"), &proof, repo, &fields("x"))
        .await
        .expect_err("no source");
    assert_reason(&err, ErrorCode::RepositoryPathNotResolved, "ref_not_found");
    assert!(err.to_string().contains("branch=source_branch"), "{err:?}");
    let proof = open_proof(&t, &as_owner, p, ("feature", "gone"), false)
        .await
        .expect("allowed");
    let err = review
        .open(&request("req-b"), &proof, repo, &fields("x"))
        .await
        .expect_err("no target");
    assert!(err.to_string().contains("branch=target_branch"), "{err:?}");
    assert_eq!(
        count(&t, "SELECT count(*) AS n FROM forge.merge_requests").await,
        0
    );
    // A merge request whose source branch was deleted stays; it cannot be
    // reopened until the branch is back (ADR-0024 §1).
    let proof = open_proof(&t, &as_owner, p, ("feature", "main"), false)
        .await
        .expect("allowed");
    review
        .open(&request("req-c"), &proof, repo, &fields("x"))
        .await
        .expect("open");
    let close = stored_proof(
        &t,
        &review,
        &as_owner,
        Action::MergeRequestClose,
        p,
        1,
        MergeRequestEdit::default(),
    )
    .await
    .expect("close proof");
    let closed = review
        .close(&request("req-d"), &close)
        .await
        .expect("close");
    assert_eq!(closed, StateOutcome::Changed);
    branches.delete("feature");
    assert!(review.merge_request(p, 1).await.expect("get").is_some());
    let reopen = stored_proof(
        &t,
        &review,
        &as_owner,
        Action::MergeRequestReopen,
        p,
        1,
        MergeRequestEdit::default(),
    )
    .await
    .expect("reopen proof");
    let err = review
        .reopen(&request("req-e"), &reopen, repo)
        .await
        .expect_err("source gone");
    assert_reason(&err, ErrorCode::RepositoryPathNotResolved, "ref_not_found");
    t.drop_now().await;
}

#[tokio::test]
async fn bad_input_and_mismatched_proofs_are_refused_before_any_write() {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let web = project(&t, owner, &[]).await;
    let p = web.project_id;
    let repo = ready(web.repository_id);
    let review = Review::new(t.db.clone(), Branches::with(&["main", "feature"]));
    let as_owner = session(owner);
    let proof = open_proof(&t, &as_owner, p, ("feature", "main"), false)
        .await
        .expect("allowed");
    let err = review
        .open(&request("req-a"), &proof, repo, &fields(" "))
        .await
        .expect_err("blank title");
    assert_reason(&err, ErrorCode::RequestInvalid, "param_invalid");
    // A repository still provisioning cannot be looked up.
    let provisioning = forge_core::ProjectRepository {
        id: web.repository_id,
        state: forge_core::RepositoryState::Provisioning,
    };
    let err = review
        .open(&request("req-b"), &proof, provisioning, &fields("x"))
        .await
        .expect_err("not ready");
    assert_eq!(err.code(), ErrorCode::RepositoryNotReady);
    // A create's proof cannot drive a close, nor the reverse shape.
    let err = review
        .close(&request("req-c"), &proof)
        .await
        .expect_err("wrong action");
    assert_reason(&err, ErrorCode::Internal, "proof_action_mismatch");
    review
        .open(&request("req-d"), &proof, repo, &fields("x"))
        .await
        .expect("open");
    let edit = stored_proof(
        &t,
        &review,
        &as_owner,
        Action::MergeRequestUpdate,
        p,
        1,
        MergeRequestEdit::default(),
    )
    .await
    .expect("edit proof");
    let err = review
        .open(&request("req-e"), &edit, repo, &fields("x"))
        .await
        .expect_err("wrong action");
    assert_reason(&err, ErrorCode::Internal, "proof_action_mismatch");
    assert_eq!(
        count(&t, "SELECT count(*) AS n FROM forge.merge_requests").await,
        1
    );
    t.drop_now().await;
}

/// ADR-0024 §12: another Developer may retitle the author's merge request
/// but the draft flag is the author's or a Maintainer's — decided again
/// inside the transaction (a proof taken while the caller was Maintainer
/// no longer holds once demoted).
#[tokio::test]
async fn a_demoted_maintainer_can_no_longer_toggle_anothers_draft() {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let dev = user(&t, "dave").await;
    let mia = user(&t, "mia").await;
    let web = project(
        &t,
        owner,
        &[(dev, Role::Developer), (mia, Role::Maintainer)],
    )
    .await;
    let p = web.project_id;
    let repo = ready(web.repository_id);
    let review = Review::new(t.db.clone(), Branches::with(&["main", "feature"]));
    let proof = open_proof(&t, &session(dev), p, ("feature", "main"), true)
        .await
        .expect("allowed");
    review
        .open(&request("req-open"), &proof, repo, &fields("x"))
        .await
        .expect("open");
    let ready_flag = MergeRequestEdit {
        target_branch: None,
        draft: Some(false),
    };
    let proof = stored_proof(
        &t,
        &review,
        &session(mia),
        Action::MergeRequestUpdate,
        p,
        1,
        ready_flag,
    )
    .await
    .expect("a maintainer may");
    let demoted = projects::change_member(
        &t.db,
        &system("req-demote"),
        p,
        mia,
        Role::Maintainer,
        Role::Developer,
    )
    .await
    .expect("demote");
    assert!(matches!(
        demoted,
        forge_db::groups::MemberChange::Changed { .. }
    ));
    let err = review
        .edit(&request("req-edit"), &proof, repo, &EditFields::default())
        .await
        .expect_err("demoted meanwhile");
    assert_reason(&err, ErrorCode::Forbidden, "foreign_draft");
    let still = review
        .merge_request(p, 1)
        .await
        .expect("get")
        .expect("there");
    assert!(still.draft);
    t.drop_now().await;
}
