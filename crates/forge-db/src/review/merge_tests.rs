//! Merge attempts against a migrated database (M3 R4, ADR-0024 §4.2): the
//! request, the gate, the fence, the end of an attempt, the guard, the lock
//! and the credential read again by the job. `decide` is a plain closure
//! here (forge-review passes the reconfirmed decision and
//! `evaluate_merge()`, tested there with real git).

use std::time::Duration;

use forge_core::{MergeMethod, MergeState, ObjectId, RequestId, Scope};
use forge_errors::ErrorCode;

use super::merges::{
    self, end_merge, fence_merge, lock_merges, merge_attempt, merge_gate_decided,
    request_merge_mr_decided, stuck_merges, CredentialRef, CredentialState, GateOutcome, GateStep,
    NewMerge,
};
use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::tokens::{self, NewToken};
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(Actor::User(user), RequestId::parse(request).expect("id"))
}

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

fn ask(credential: CredentialRef) -> NewMerge {
    NewMerge {
        method: MergeMethod::Squash,
        head_sha: oid('a'),
        message: None,
        delete_source_branch: false,
        credential,
    }
}

struct World {
    t: FreshDb,
    alice: UserId,
    web: CreatedProject,
    mr: StoredMergeRequest,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &system("req-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    let new = NewMergeRequest {
        title: "Merge feature".to_owned(),
        description: "synthetic".to_owned(),
        source_branch: RefName::parse("refs/heads/feature").expect("ref"),
        target_branch: RefName::parse("refs/heads/main").expect("ref"),
        draft: false,
    };
    let (mr, ()) = open_mr_decided(
        &t.db,
        &as_user(alice.user_id, "req-open"),
        web.project_id,
        alice.user_id,
        &new,
        |_| Ok(()),
    )
    .await
    .expect("open");
    World {
        t,
        alice: alice.user_id,
        web,
        mr,
    }
}

async fn request(w: &World, req: &str) -> Result<merges::StoredMerge, ForgeError> {
    request_merge_mr_decided(
        &w.t.db,
        &as_user(w.alice, req),
        (w.web.project_id, w.mr.iid),
        w.alice,
        &ask(CredentialRef::Session(1)),
        |_| Ok(()),
    )
    .await
    .map(|(m, ())| m)
}

fn assert_code(e: &ForgeError, code: ErrorCode) {
    assert_eq!(e.code(), code, "{e:?}");
}

/// ADR-0024 §4.2 step 1: the request writes the `queued` attempt and its
/// job; a second one while it is live is FG-REVIEW-0011 — also when both
/// race (the merge request's row lock serializes them, the partial unique
/// index backs it).
#[tokio::test]
async fn two_merges_of_one_merge_request_at_once_let_exactly_one_win() {
    let w = world().await;
    let (a, b) = tokio::join!(request(&w, "req-merge-a"), request(&w, "req-merge-b"));
    let (won, lost) = match (a, b) {
        (Ok(m), Err(e)) | (Err(e), Ok(m)) => (m, e),
        other => panic!("exactly one must win: {other:?}"),
    };
    assert_code(&lost, ErrorCode::MergeInProgress);
    assert_eq!(won.state, MergeState::Queued);
    assert_eq!(won.target_branch.as_str(), "refs/heads/main");
    let jobs: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, dedup_key FROM forge.jobs WHERE kind = 'merge_request.merge'")
            .fetch_all(&mut w.t.superuser().await)
            .await
            .expect("jobs");
    assert_eq!(
        jobs,
        [(
            "merge_request.merge".to_owned(),
            format!("mr_merge:{}", won.id)
        )]
    );
    w.t.drop_now().await;
}

/// ADR-0024 §4.2 "マージの実行中": a close, a retarget and a draft toggle
/// wait for the live attempt (FG-REVIEW-0011); a title edit does not.
#[tokio::test]
async fn a_live_merge_refuses_close_retarget_and_draft_but_not_a_title() {
    let w = world().await;
    let attempt = request(&w, "req-merge").await.expect("merge");
    let p = w.web.project_id;
    let close = close_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-close"),
        p,
        w.mr.iid,
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect_err("close waits");
    assert_code(&close, ErrorCode::MergeInProgress);
    let draft = MergeRequestPatch {
        draft: Some(true),
        ..MergeRequestPatch::default()
    };
    let err = edit_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-draft"),
        p,
        w.mr.iid,
        w.alice,
        &draft,
        |_| Ok(()),
    )
    .await
    .expect_err("draft waits");
    assert_code(&err, ErrorCode::MergeInProgress);
    let title = MergeRequestPatch {
        title: Some("Renamed".to_owned()),
        ..MergeRequestPatch::default()
    };
    let (title_outcome, _, ()) = edit_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-title"),
        p,
        w.mr.iid,
        w.alice,
        &title,
        |_| Ok(()),
    )
    .await
    .expect("a title edit is no merge input");
    assert!(matches!(title_outcome, EditOutcome::Changed(_)));
    // Once the attempt ends, the close goes through.
    assert!(end_merge(
        &w.t.db,
        attempt.id,
        &[MergeState::Queued],
        MergeState::Superseded,
        "test",
        None
    )
    .await
    .expect("end"));
    let (closed, _, ()) = close_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-close-2"),
        p,
        w.mr.iid,
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("close after the merge ended");
    assert_eq!(closed, StateOutcome::Changed);
    w.t.drop_now().await;
}

/// A refusal inside the gate ends the attempt `superseded` with the
/// refusal's reason and code, in the same transaction; a prepared attempt
/// is fenced once; a gate on an attempt already moved does nothing.
#[tokio::test]
async fn the_gate_supersedes_on_refusal_and_the_fence_is_taken_once() {
    let w = world().await;
    let refused = request(&w, "req-merge-1").await.expect("merge");
    let err = merge_gate_decided(
        &w.t.db,
        &system("req-gate-1"),
        refused.id,
        GateStep::Check,
        |_| {
            Err::<(), _>(
                ForgeError::new(ErrorCode::HeadChanged).with_context("reason", "source_moved"),
            )
        },
    )
    .await
    .expect_err("refused");
    assert_code(&err, ErrorCode::HeadChanged);
    let stored = merge_attempt(&w.t.db, refused.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(stored.state, MergeState::Superseded);
    assert_eq!(stored.reason.as_deref(), Some("source_moved"));
    assert_eq!(stored.error_code.as_deref(), Some("FG-REVIEW-0008"));
    assert!(stored.finished_at.is_some());

    let attempt = request(&w, "req-merge-2").await.expect("merge again");
    let step = GateStep::Prepare {
        expected_old: oid('b'),
        new_sha: oid('c'),
    };
    let done = merge_gate_decided(
        &w.t.db,
        &system("req-gate-2"),
        attempt.id,
        step.clone(),
        |g| {
            assert_eq!(g.live.as_ref().map(|l| l.id), Some(attempt.id));
            assert_eq!(g.credential.credential, CredentialState::Missing);
            Ok(())
        },
    )
    .await
    .expect("prepare");
    assert!(matches!(done, GateOutcome::Decided(())));
    // A second prepare finds it moved.
    let again = merge_gate_decided(&w.t.db, &system("req-gate-3"), attempt.id, step, |_| Ok(()))
        .await
        .expect("moved");
    assert!(matches!(again, GateOutcome::Moved(MergeState::Prepared)));
    let (a, b) = tokio::join!(
        fence_merge(&w.t.db, attempt.id),
        fence_merge(&w.t.db, attempt.id)
    );
    assert_eq!(
        [a.expect("fence a"), b.expect("fence b")]
            .iter()
            .filter(|x| **x)
            .count(),
        1,
        "exactly one fence"
    );
    let executing = merge_attempt(&w.t.db, attempt.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(executing.state, MergeState::Executing);
    assert_eq!(executing.expected_old, Some(oid('b')));
    assert_eq!(executing.new_sha, Some(oid('c')));
    assert!(merges::executing_elapsed(&w.t.db, attempt.id)
        .await
        .expect("elapsed")
        .is_some());
    w.t.drop_now().await;
}

/// The guard (`merge_request_merges_guard`): a final attempt never changes,
/// a live one only moves forward, the identity and the set-once columns
/// never change — even for a direct UPDATE.
#[tokio::test]
async fn the_guard_keeps_the_state_machine_even_for_direct_updates() {
    let w = world().await;
    let attempt = request(&w, "req-merge").await.expect("merge");
    let mut su = w.t.superuser().await;
    for (statement, why) in [
        ("UPDATE forge.merge_request_merges SET state = 'applied', executing_at = now(), expected_old = repeat('b', 40), new_sha = repeat('c', 40), finished_at = now() WHERE id = $1", "queued never becomes applied"),
        ("UPDATE forge.merge_request_merges SET head_sha = repeat('d', 40) WHERE id = $1", "identity"),
        ("UPDATE forge.merge_request_merges SET method = 'fast_forward' WHERE id = $1", "identity"),
    ] {
        let err = sqlx::query(statement)
            .bind(attempt.id)
            .execute(&mut su)
            .await
            .expect_err(why);
        assert_eq!(err.as_database_error().and_then(|d| d.code()).as_deref(), Some("XF001"), "{why}");
    }
    assert!(end_merge(
        &w.t.db,
        attempt.id,
        &[MergeState::Queued],
        MergeState::Failed,
        "test",
        Some(ErrorCode::MergeExecutionFailed)
    )
    .await
    .expect("end"));
    let err = sqlx::query("UPDATE forge.merge_request_merges SET reason = 'other' WHERE id = $1")
        .bind(attempt.id)
        .execute(&mut su)
        .await
        .expect_err("final");
    assert_eq!(
        err.as_database_error().and_then(|d| d.code()).as_deref(),
        Some("XF001")
    );
    // A second end from a live state does nothing.
    assert!(!end_merge(
        &w.t.db,
        attempt.id,
        &[MergeState::Queued],
        MergeState::Superseded,
        "test",
        None
    )
    .await
    .expect("no-op"));
    w.t.drop_now().await;
}

/// The reconciliation's scan: a `queued` attempt idle past the bound and an
/// `executing` one past its own bound; nothing younger.
#[tokio::test]
async fn stuck_merges_lists_idle_live_attempts_only() {
    let w = world().await;
    let attempt = request(&w, "req-merge").await.expect("merge");
    let none = stuck_merges(
        &w.t.db,
        Duration::from_secs(600),
        Duration::from_secs(600),
        10,
    )
    .await
    .expect("scan");
    assert!(none.is_empty());
    let mut su = w.t.superuser().await;
    sqlx::query("UPDATE forge.merge_request_merges SET updated_at = now() - interval '20 minutes' WHERE id = $1")
        .bind(attempt.id)
        .execute(&mut su)
        .await
        .expect("age");
    let stuck = stuck_merges(
        &w.t.db,
        Duration::from_secs(600),
        Duration::from_secs(600),
        10,
    )
    .await
    .expect("scan");
    assert_eq!(stuck.iter().map(|s| s.id).collect::<Vec<_>>(), [attempt.id]);
    w.t.drop_now().await;
}

/// The merge lock (`merge:<repo_id>`) is held by one job at a time and its
/// holder can prove it still stands; another repository is not blocked.
#[tokio::test]
async fn the_merge_lock_is_exclusive_per_repository_and_provably_alive() {
    let w = world().await;
    let repo = w.web.repository_id;
    let mut held = lock_merges(&w.t.db, repo, Duration::from_secs(5))
        .await
        .expect("lock");
    held.alive().await.expect("alive");
    let err = lock_merges(&w.t.db, repo, Duration::from_millis(200))
        .await
        .expect_err("busy");
    assert_code(&err, ErrorCode::DbQueryFailed);
    let other = lock_merges(
        &w.t.db,
        forge_core::RepoId::new(repo.get() + 1000),
        Duration::from_secs(5),
    )
    .await
    .expect("another repository");
    other.release().await;
    held.release().await;
    let again = lock_merges(&w.t.db, repo, Duration::from_secs(5))
        .await
        .expect("released");
    again.release().await;
    w.t.drop_now().await;
}

/// ADR-0024 §8.5: the tokens CHECK accepts `merge_execute` (R4 widens it);
/// the job reads the token again — scopes, agent flag, revocation.
#[tokio::test]
async fn a_merge_execute_token_is_stored_and_read_again_with_its_revocation() {
    let w = world().await;
    let issued = tokens::issue(
        &w.t.db,
        &as_user(w.alice, "req-token"),
        &NewToken {
            user: w.alice,
            name: "merge".to_owned(),
            scopes: vec![Scope::MergeExecute],
            expires_in: None,
            is_agent: false,
        },
    )
    .await
    .expect("issue a merge_execute token");
    let read = |req: &'static str| {
        let db = w.t.db.clone();
        let (p, iid, alice, id) = (w.web.project_id, w.mr.iid, w.alice, issued.token_id);
        async move {
            let attempt = request_merge_mr_decided(
                &db,
                &as_user(alice, req),
                (p, iid),
                alice,
                &ask(CredentialRef::Token(id)),
                |g| Ok(g.credential.clone()),
            )
            .await;
            attempt
        }
    };
    let (attempt, facts) = read("req-merge-1").await.expect("merge");
    assert_eq!(
        facts.credential,
        CredentialState::Token {
            scopes: vec![Scope::MergeExecute],
            is_agent: false,
            usable: true
        }
    );
    let _revoked = tokens::revoke(&w.t.db, &as_user(w.alice, "req-revoke"), issued.token_id)
        .await
        .expect("revoke");
    let gate = merge_gate_decided(
        &w.t.db,
        &system("req-gate"),
        attempt.id,
        GateStep::Check,
        |g| Ok(g.credential.clone()),
    )
    .await
    .expect("gate");
    let GateOutcome::Decided(after) = gate else {
        panic!("decided");
    };
    assert!(matches!(
        after.credential,
        CredentialState::Token { usable: false, .. }
    ));
    w.t.drop_now().await;
}
