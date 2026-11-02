//! Merge requests under concurrency and at the database's backstops (M3
//! R1a review): the same pair opened at once, close racing reopen, the
//! revision rule of the state guard and a CHECK mapped to the request.

use std::collections::BTreeMap;

use forge_core::RequestId;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::User(user),
        RequestId::parse(request).expect("request id"),
    )
}

fn branch(name: &str) -> RefName {
    RefName::parse(&format!("refs/heads/{name}")).expect("branch")
}

fn new_mr(source: &str, target: &str) -> NewMergeRequest {
    NewMergeRequest {
        title: format!("Merge {source}"),
        description: String::new(),
        source_branch: branch(source),
        target_branch: branch(target),
        draft: false,
    }
}

async fn web(t: &FreshDb) -> (UserId, ProjectId) {
    let system = ChangeContext::new(Actor::System, RequestId::parse("req-seed").expect("id"));
    let alice = users::create(
        &t.db,
        &system,
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &system,
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    (alice.user_id, web.project_id)
}

async fn count(t: &FreshDb, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count")
}

/// The same (source, target) opened N times at once: exactly one opens, the
/// rest are FG-REVIEW-0001, one `opened` event, and the refused ones draw no
/// number (the counter is at 1).
#[tokio::test]
async fn concurrent_opens_of_the_same_pair_open_exactly_one() {
    const N: usize = 6;
    let t = FreshDb::migrated().await;
    let (alice, p) = web(&t).await;
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..N {
        let db = t.db.clone();
        tasks.spawn(async move {
            let ctx = as_user(alice, &format!("req-same-{i}"));
            open_mr_decided(&db, &ctx, p, alice, &new_mr("feature", "main"), |_| Ok(()))
                .await
                .map(|(stored, ())| stored.iid)
        });
    }
    let (mut opened, mut refused) = (Vec::new(), 0);
    while let Some(done) = tasks.join_next().await {
        match done.expect("task") {
            Ok(iid) => opened.push(iid),
            Err(e) => {
                assert_reason(&e, ErrorCode::DuplicateOpenMergeRequest, "open_exists");
                refused += 1;
            }
        }
    }
    assert_eq!((opened, refused), (vec![1], N - 1));
    let events = count(
        &t,
        "SELECT count(*) FROM forge.events WHERE event_type = 'merge_request.opened'",
    )
    .await;
    assert_eq!(events, 1);
    let last = count(&t, "SELECT last_iid FROM forge.merge_request_counters").await;
    assert_eq!(last, 1, "no gap: refused opens draw no number");
    t.drop_now().await;
}

/// Closes and reopens of one merge request at once: each change is
/// serialized by the row lock, writes one event, and counts once in the
/// revision; the final state is the one the last change left.
#[tokio::test]
async fn concurrent_close_and_reopen_stay_consistent() {
    const N: usize = 8;
    let t = FreshDb::migrated().await;
    let (alice, p) = web(&t).await;
    open_mr_decided(
        &t.db,
        &as_user(alice, "req-open"),
        p,
        alice,
        &new_mr("feature", "main"),
        |_| Ok(()),
    )
    .await
    .expect("open");
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..N {
        let db = t.db.clone();
        tasks.spawn(async move {
            let ctx = as_user(alice, &format!("req-flip-{i}"));
            let outcome = if i % 2 == 0 {
                close_mr_decided(&db, &ctx, p, 1, alice, |_| Ok(())).await
            } else {
                reopen_mr_decided(&db, &ctx, p, 1, alice, |_| Ok(())).await
            };
            outcome.map(|(done, stored, ())| (done, stored.revision))
        });
    }
    let mut changed = 0_i64;
    let mut revisions = BTreeMap::new();
    while let Some(done) = tasks.join_next().await {
        let (outcome, revision) = done.expect("task").expect("close or reopen");
        if outcome == StateOutcome::Changed {
            changed += 1;
            assert!(
                revisions.insert(revision, ()).is_none(),
                "revision {revision} twice"
            );
        }
    }
    let events = count(
        &t,
        "SELECT count(*) FROM forge.events WHERE event_type IN \
         ('merge_request.closed', 'merge_request.reopened')",
    )
    .await;
    assert_eq!(events, changed, "one event per change");
    let stored = get(&t.db, p, 1).await.expect("get").expect("there");
    assert_eq!(stored.revision, 1 + changed, "every change counted once");
    let last_event: String = sqlx::query_scalar(
        "SELECT event_type FROM forge.events WHERE event_type LIKE 'merge_request.%' \
         ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("last event");
    let expected = match stored.state {
        MergeRequestState::Closed => "merge_request.closed",
        MergeRequestState::Open if changed == 0 => "merge_request.opened",
        MergeRequestState::Open => "merge_request.reopened",
        MergeRequestState::Merged => panic!("never merged here"),
    };
    assert_eq!(last_event, expected);
    t.drop_now().await;
}

/// The state guard: a change that does not add one to the revision is
/// refused (XF001), whoever writes it.
#[tokio::test]
async fn a_change_without_its_revision_step_is_refused() {
    let t = FreshDb::migrated().await;
    let (alice, p) = web(&t).await;
    let (mr, ()) = open_mr_decided(
        &t.db,
        &as_user(alice, "req-open"),
        p,
        alice,
        &new_mr("feature", "main"),
        |_| Ok(()),
    )
    .await
    .expect("open");
    let id = mr.id.get();
    for sql in [
        format!("UPDATE forge.merge_requests SET title = 'x' WHERE id = {id}"),
        format!(
            "UPDATE forge.merge_requests SET title = 'x', revision = revision + 2 WHERE id = {id}"
        ),
    ] {
        let err = sqlx::raw_sql(&sql)
            .execute(&mut t.superuser().await)
            .await
            .expect_err("guarded");
        assert_eq!(
            err.as_database_error().and_then(|d| d.code()).as_deref(),
            Some("XF001"),
            "{sql}"
        );
    }
    t.drop_now().await;
}

/// A target equal to the source reaching the database (authorize() and the
/// route refuse it first) is the request's error, FG-WEB-0006 from the
/// CHECK merge_requests_branches_differ — not a generic database failure.
#[tokio::test]
async fn an_edit_onto_the_source_branch_is_refused_as_a_request_error() {
    let t = FreshDb::migrated().await;
    let (alice, p) = web(&t).await;
    open_mr_decided(
        &t.db,
        &as_user(alice, "req-open"),
        p,
        alice,
        &new_mr("feature", "main"),
        |_| Ok(()),
    )
    .await
    .expect("open");
    let patch = MergeRequestPatch {
        target_branch: Some(branch("feature")),
        ..MergeRequestPatch::default()
    };
    let err = edit_mr_decided(
        &t.db,
        &as_user(alice, "req-same"),
        p,
        1,
        alice,
        &patch,
        |_| Ok(()),
    )
    .await
    .expect_err("the source");
    assert_reason(&err, ErrorCode::RequestInvalid, "param_invalid");
    assert!(
        err.to_string().contains("rule=source_equals_target"),
        "{err}"
    );
    let stored = get(&t.db, p, 1).await.expect("get").expect("there");
    assert_eq!(stored.target_branch.as_str(), "refs/heads/main");
    t.drop_now().await;
}
