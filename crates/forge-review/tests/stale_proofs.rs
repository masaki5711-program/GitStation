#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The branch checks of reopen and retarget never trust the proof's view of
//! the merge request (M3 R1a review, authz MEDIUM 1): a close or a retarget
//! that lands between the registry's decision and the command cannot let a
//! deleted branch through.

mod common;

use common::*;
use forge_auth::{Action, MergeRequestEdit};
use forge_core::{MergeRequestState, Role};
use forge_db::test_support::{assert_reason, FreshDb};
use forge_errors::ErrorCode;
use forge_review::{EditFields, NewFields, Review, StateOutcome};

fn fields() -> NewFields {
    NewFields {
        title: "x".to_owned(),
        description: String::new(),
    }
}

/// The reopen proof was taken while the merge request was open (a reopen
/// of an open one is a no-op, so nothing was checked then); it is closed and
/// its source branch deleted before the command runs: the command checks
/// the branches of the row it reads, and refuses.
#[tokio::test]
async fn a_reopen_proof_taken_while_open_still_checks_the_branches_after_a_close() {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let dev = user(&t, "dave").await;
    let web = project(&t, owner, &[(dev, Role::Developer)]).await;
    let p = web.project_id;
    let repo = ready(web.repository_id);
    let branches = Branches::with(&["main", "feature"]);
    let review = Review::new(t.db.clone(), branches.clone());
    let as_dev = session(dev);
    let open = open_proof(&t, &as_dev, p, ("feature", "main"), false)
        .await
        .expect("allowed");
    review
        .open(&request("req-open"), &open, repo, &fields())
        .await
        .expect("open");
    let stale = stored_proof(
        &t,
        &review,
        &as_dev,
        Action::MergeRequestReopen,
        p,
        1,
        MergeRequestEdit::default(),
    )
    .await
    .expect("reopen proof while open");
    let close = stored_proof(
        &t,
        &review,
        &as_dev,
        Action::MergeRequestClose,
        p,
        1,
        MergeRequestEdit::default(),
    )
    .await
    .expect("close proof");
    let (closed, _) = review
        .close(&request("req-close"), &close)
        .await
        .expect("close");
    assert_eq!(closed, StateOutcome::Changed);
    branches.delete("feature");
    let err = review
        .reopen(&request("req-reopen"), &stale, repo)
        .await
        .expect_err("the source branch is gone");
    assert_reason(&err, ErrorCode::RepositoryPathNotResolved, "ref_not_found");
    let still = review
        .merge_request(p, 1)
        .await
        .expect("get")
        .expect("there");
    assert_eq!(still.state, MergeRequestState::Closed);
    t.drop_now().await;
}

/// The edit proof saw `release` as the current target (so retargeting to
/// it looked like no change); another edit moves the target back to `main`
/// and `release` is deleted before the command runs: the new target is
/// checked anyway, and nothing is written.
#[tokio::test]
async fn a_retarget_is_checked_even_when_the_proof_saw_it_as_the_current_target() {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let web = project(&t, owner, &[]).await;
    let p = web.project_id;
    let repo = ready(web.repository_id);
    let branches = Branches::with(&["main", "feature", "release"]);
    let review = Review::new(t.db.clone(), branches.clone());
    let me = session(owner);
    let open = open_proof(&t, &me, p, ("feature", "main"), false)
        .await
        .expect("allowed");
    review
        .open(&request("req-open"), &open, repo, &fields())
        .await
        .expect("open");
    let to = |target: &str| MergeRequestEdit {
        target_branch: Some(branch(target)),
        draft: None,
    };
    let first = stored_proof(
        &t,
        &review,
        &me,
        Action::MergeRequestUpdate,
        p,
        1,
        to("release"),
    )
    .await
    .expect("retarget proof");
    let (_changed, _) = review
        .edit(&request("req-first"), &first, repo, &EditFields::default())
        .await
        .expect("to release");
    let stale = stored_proof(
        &t,
        &review,
        &me,
        Action::MergeRequestUpdate,
        p,
        1,
        to("release"),
    )
    .await
    .expect("a proof that sees release as current");
    let back = stored_proof(
        &t,
        &review,
        &me,
        Action::MergeRequestUpdate,
        p,
        1,
        to("main"),
    )
    .await
    .expect("back proof");
    let (_changed, _) = review
        .edit(&request("req-back"), &back, repo, &EditFields::default())
        .await
        .expect("back to main");
    branches.delete("release");
    let err = review
        .edit(&request("req-stale"), &stale, repo, &EditFields::default())
        .await
        .expect_err("release is gone");
    assert_reason(&err, ErrorCode::RepositoryPathNotResolved, "ref_not_found");
    let now = review
        .merge_request(p, 1)
        .await
        .expect("get")
        .expect("there");
    assert_eq!(now.target_branch.as_str(), "refs/heads/main");
    t.drop_now().await;
}
