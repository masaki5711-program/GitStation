//! Merge request versions against a migrated database (M3 R1b): the
//! version job's record, the purge marks, the reopen guard, the jobs an
//! open, a reopen and a push enqueue (the subscriber).

use forge_core::{
    ObjectId, PushRecord, PushRecorder, PushUpdateKind, PushedRef, Pusher, RequestId,
};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::subscribers::{self, Progress, Start};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::User(user),
        RequestId::parse(request).expect("request id"),
    )
}

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

fn new_mr(source: &str) -> NewMergeRequest {
    NewMergeRequest {
        title: format!("Merge {source}"),
        description: String::new(),
        source_branch: RefName::parse(&format!("refs/heads/{source}")).expect("branch"),
        target_branch: RefName::parse("refs/heads/main").expect("branch"),
        draft: false,
    }
}

async fn web(t: &FreshDb) -> (UserId, CreatedProject) {
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &system("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    (alice.user_id, web)
}

async fn open(t: &FreshDb, who: UserId, p: ProjectId, source: &str) -> StoredMergeRequest {
    let request = format!("req-open-{source}");
    open_mr_decided(
        &t.db,
        &as_user(who, &request),
        p,
        who,
        &new_mr(source),
        |_| Ok(()),
    )
    .await
    .map(|(stored, ())| stored)
    .expect("open")
}

/// (dedup_key, payload) of the version jobs, oldest first.
async fn refresh_jobs(t: &FreshDb) -> Vec<(String, serde_json::Value)> {
    sqlx::query_as(
        "SELECT dedup_key, payload FROM forge.jobs WHERE kind = 'merge_request.refresh' \
         ORDER BY id",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("jobs")
}

async fn count(t: &FreshDb, sql: &str, request: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(request)
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count")
}

#[tokio::test]
async fn versions_are_numbered_per_merge_request_and_a_rerun_writes_nothing() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let mr = open(&t, alice, web.project_id, "feature").await;
    let first = record_mr_version(&t.db, &system("req-v1"), mr.id, &oid('a'), Some(alice))
        .await
        .expect("v1");
    assert_eq!(first, VersionOutcome::Added { version: 1 });
    // The job ran again (a lost lease): nothing written, no second event.
    let again = record_mr_version(
        &t.db,
        &system("req-v1-again"),
        mr.id,
        &oid('a'),
        Some(alice),
    )
    .await
    .expect("rerun");
    assert_eq!(again, VersionOutcome::AlreadyHead { version: Some(1) });
    let second = record_mr_version(&t.db, &system("req-v2"), mr.id, &oid('b'), None)
        .await
        .expect("v2");
    assert_eq!(second, VersionOutcome::Added { version: 2 });
    let stored = list_versions(&t.db, mr.id).await.expect("versions");
    let heads: Vec<(i64, &str, Option<UserId>)> = stored
        .iter()
        .map(|v| (v.version_no, v.head_sha.as_str(), v.pushed_by))
        .collect();
    let (a, b) = ("a".repeat(40), "b".repeat(40));
    assert_eq!(heads, [(1, a.as_str(), Some(alice)), (2, b.as_str(), None)]);
    let row = get(&t.db, web.project_id, mr.iid)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(row.head_sha.as_deref(), Some(b.as_str()));
    let events: Vec<(String, String, Option<String>, serde_json::Value)> = sqlx::query_as(
        "SELECT event_type, actor_kind, dedup_key, payload FROM forge.events \
         WHERE event_type = 'merge_request.pushed' ORDER BY id",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("events");
    assert_eq!(events.len(), 2, "one event per version: {events:?}");
    assert_eq!(events[0].1, "system");
    assert_eq!(
        events[0].2.as_deref(),
        Some(format!("merge_request_pushed:{}:{a}", mr.id.get()).as_str())
    );
    assert_eq!(events[0].3["version"], 1);
    assert_eq!(events[0].3["pushed_by"], alice.get());
    assert_eq!(events[1].3["previous_head"], a.as_str());
    let audits = count(
        &t,
        "SELECT count(*) FROM forge.audit_log WHERE request_id = $1",
        "req-v1-again",
    )
    .await;
    assert_eq!(audits, 0, "a rerun writes no audit row");
    t.drop_now().await;
}

/// The source going back to an older head makes that version the head
/// again (audit-only), adds none; a closed merge request gets nothing.
#[tokio::test]
async fn an_older_head_is_restored_and_a_closed_merge_request_gets_no_version() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let mr = open(&t, alice, web.project_id, "feature").await;
    for (request, head) in [("req-a", 'a'), ("req-b", 'b')] {
        record_mr_version(&t.db, &system(request), mr.id, &oid(head), None)
            .await
            .expect("version");
    }
    let back = record_mr_version(&t.db, &system("req-back"), mr.id, &oid('a'), None)
        .await
        .expect("back");
    assert_eq!(back, VersionOutcome::Restored { version: 1 });
    assert_eq!(
        list_versions(&t.db, mr.id).await.expect("versions").len(),
        2
    );
    let restored = count(
        &t,
        "SELECT count(*) FROM forge.audit_log WHERE request_id = $1 \
         AND action = 'merge_request.head_restored'",
        "req-back",
    )
    .await;
    assert_eq!(restored, 1);
    let events = count(
        &t,
        "SELECT count(*) FROM forge.events WHERE request_id = $1",
        "req-back",
    )
    .await;
    assert_eq!(events, 0, "a restored head publishes no event");
    let _closed = close_mr_decided(
        &t.db,
        &as_user(alice, "req-close"),
        web.project_id,
        mr.iid,
        alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
    let closed = record_mr_version(&t.db, &system("req-c"), mr.id, &oid('c'), None)
        .await
        .expect("closed");
    assert_eq!(closed, VersionOutcome::NotOpen(MergeRequestState::Closed));
    t.drop_now().await;
}

/// ADR-0024 §2: an open merge request's current head is never purged
/// (FG-REVIEW-0002); older versions are; a purged head never comes back.
#[tokio::test]
async fn purge_of_an_open_merge_requests_current_head_is_refused() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let mr = open(&t, alice, p, "feature").await;
    for (request, head) in [("req-a", 'a'), ("req-b", 'b')] {
        record_mr_version(&t.db, &system(request), mr.id, &oid(head), None)
            .await
            .expect("version");
    }
    let err = purge_mr_version_refs_decided(
        &t.db,
        &as_user(alice, "req-purge-all"),
        p,
        mr.iid,
        alice,
        None,
        |_| Ok(()),
    )
    .await
    .expect_err("open head");
    assert_eq!(err.code(), ErrorCode::PurgePreconditionFailed);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "purge_requires_closed_or_rewritten"));
    // An unknown number is a request error.
    let err = purge_mr_version_refs_decided(
        &t.db,
        &as_user(alice, "req-purge-9"),
        p,
        mr.iid,
        alice,
        Some(&[9]),
        |_| Ok(()),
    )
    .await
    .expect_err("unknown");
    assert_eq!(err.code(), ErrorCode::RequestInvalid);
    // The older version may go (the branch was rewritten past it).
    let (done, ()) = purge_mr_version_refs_decided(
        &t.db,
        &as_user(alice, "req-purge-1"),
        p,
        mr.iid,
        alice,
        Some(&[1]),
        |_| Ok(()),
    )
    .await
    .expect("purge v1");
    assert_eq!((done.purged, done.already_purged), (vec![1], vec![]));
    let audit = count(
        &t,
        "SELECT count(*) FROM forge.audit_log WHERE request_id = $1 \
         AND action = 'merge_request.version_refs_purged'",
        "req-purge-1",
    )
    .await;
    assert_eq!(audit, 1);
    // Asked again: nothing written.
    let (again, ()) = purge_mr_version_refs_decided(
        &t.db,
        &as_user(alice, "req-purge-1b"),
        p,
        mr.iid,
        alice,
        Some(&[1]),
        |_| Ok(()),
    )
    .await
    .expect("again");
    assert_eq!((again.purged, again.already_purged), (vec![], vec![1]));
    // A force push back to the purged commit does not make it the head.
    let back = record_mr_version(&t.db, &system("req-back"), mr.id, &oid('a'), None)
        .await
        .expect("back");
    assert_eq!(back, VersionOutcome::PurgedHead { version: 1 });
    let trace = count(
        &t,
        "SELECT count(*) FROM forge.audit_log WHERE request_id = $1 \
         AND action = 'merge_request.purged_head_pushed'",
        "req-back",
    )
    .await;
    assert_eq!(trace, 1, "the purged head pushed back leaves an audit row");
    let row = get(&t.db, p, mr.iid).await.expect("get").expect("row");
    assert_eq!(
        row.head_sha.as_deref(),
        Some("b".repeat(40).as_str()),
        "head unchanged"
    );
    let marks: Vec<bool> = list_versions(&t.db, mr.id)
        .await
        .expect("versions")
        .iter()
        .map(|v| v.refs_purged)
        .collect();
    assert_eq!(marks, [true, false]);
    // The mark is final (the guard).
    let cleared = sqlx::query(
        "UPDATE forge.merge_request_versions SET refs_purged_at = NULL WHERE version_no = 1",
    )
    .execute(&mut t.superuser().await)
    .await;
    assert!(cleared.is_err(), "refs_purged_at is set once");
    t.drop_now().await;
}

/// ADR-0024 closing table #43: a purged merge request is reopened only when
/// its source moved off the purged commit.
#[tokio::test]
async fn reopen_at_a_purged_head_is_refused() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let mr = open(&t, alice, p, "feature").await;
    record_mr_version(&t.db, &system("req-a"), mr.id, &oid('a'), None)
        .await
        .expect("v1");
    let _closed = close_mr_decided(
        &t.db,
        &as_user(alice, "req-close"),
        p,
        mr.iid,
        alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
    let _purged = purge_mr_version_refs_decided(
        &t.db,
        &as_user(alice, "req-purge"),
        p,
        mr.iid,
        alice,
        None,
        |_| Ok(()),
    )
    .await
    .expect("purge closed");
    let err = reopen_mr_decided(
        &t.db,
        &as_user(alice, "req-reopen"),
        p,
        mr.iid,
        alice,
        Some(&oid('a')),
        |_| Ok(()),
    )
    .await
    .expect_err("same head");
    assert_eq!(err.code(), ErrorCode::PurgePreconditionFailed);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "reopen_at_purged_head"));
    let (done, _, ()) = reopen_mr_decided(
        &t.db,
        &as_user(alice, "req-reopen-2"),
        p,
        mr.iid,
        alice,
        Some(&oid('b')),
        |_| Ok(()),
    )
    .await
    .expect("moved");
    assert_eq!(done, StateOutcome::Changed);
    t.drop_now().await;
}

fn pushed(name: &str, kind: PushUpdateKind) -> PushedRef {
    let (old, new) = match kind {
        PushUpdateKind::Create => (oid('0'), oid('b')),
        PushUpdateKind::Delete => (oid('a'), oid('0')),
        PushUpdateKind::FastForward | PushUpdateKind::Force => (oid('a'), oid('b')),
    };
    PushedRef {
        name: RefName::parse(name).expect("ref"),
        old,
        new,
        kind,
        commits: Vec::new(),
        total_commits: 0,
        total_commits_partial: false,
    }
}

async fn push(t: &FreshDb, user: UserId, request: &str, push: &PushRecord) {
    let who = Pusher {
        request_id: RequestId::parse(request).expect("request id"),
        user,
        kind: forge_core::UserKind::Human,
        credential_id: None,
        source_ip: None,
        user_agent: None,
    };
    let recorder: &dyn PushRecorder = &t.db;
    let done = recorder.record_push(&who, push).await.expect("push");
    assert_eq!(done, forge_core::PushRecorded::Recorded);
}

async fn drain(t: &FreshDb) {
    crate::sequencer::run_once(
        &t.db,
        crate::sequencer::BatchSize::MAX,
        crate::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
    loop {
        match subscribers::run_once(&t.db, &MergeRequestRefresh::default())
            .await
            .expect("step")
        {
            Progress::Handled { .. } => {}
            Progress::CaughtUp => break,
            other @ (Progress::Busy
            | Progress::Disabled
            | Progress::Failed(_)
            | Progress::Stuck(_)
            | Progress::Quarantined(_)) => panic!("unexpected {other:?}"),
        }
    }
}

/// Plan 11 §11.3: an open enqueues the first version's job; a push to a
/// source branch enqueues one per open merge request from it (once, even
/// when the step runs again); a deleted branch or another branch nothing;
/// a cut ref list (`refs_truncated`) re-checks every open merge request.
#[tokio::test]
async fn pushes_open_and_reopen_enqueue_the_version_job() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    subscribers::register(&t.db, &MergeRequestRefresh::default(), Start::Head)
        .await
        .expect("register");
    let feature = open(&t, alice, p, "feature").await;
    let other = open(&t, alice, p, "other").await;
    let jobs = refresh_jobs(&t).await;
    let keys: Vec<&str> = jobs.iter().map(|j| j.0.as_str()).collect();
    let (f, o) = (feature.id.get(), other.id.get());
    assert_eq!(
        keys,
        [
            format!("mr_refresh:{f}:opened"),
            format!("mr_refresh:{o}:opened")
        ]
    );
    assert_eq!(jobs[0].1["pushed_by"], serde_json::Value::Null);

    let repo = web.repository_id;
    push(
        &t,
        alice,
        "req-push-1",
        &PushRecord {
            repository_id: repo,
            updates: vec![
                pushed("refs/heads/feature", PushUpdateKind::Force),
                pushed("refs/heads/other", PushUpdateKind::Delete),
                pushed("refs/heads/unrelated", PushUpdateKind::Create),
            ],
        },
    )
    .await;
    drain(&t).await;
    let jobs = refresh_jobs(&t).await;
    assert_eq!(jobs.len(), 3, "{jobs:?}");
    let (key, payload) = &jobs[2];
    assert!(key.starts_with(&format!("mr_refresh:{f}:push:")), "{key}");
    assert_eq!(payload["merge_request_id"], f);
    assert_eq!(payload["pushed_by"], alice.get());

    // More refs than the event lists: every open merge request.
    let many: Vec<PushedRef> = (0..=forge_core::push::MAX_LISTED_REFS)
        .map(|i| pushed(&format!("refs/heads/bulk-{i:03}"), PushUpdateKind::Create))
        .collect();
    push(
        &t,
        alice,
        "req-push-bulk",
        &PushRecord {
            repository_id: repo,
            updates: many,
        },
    )
    .await;
    drain(&t).await;
    let jobs = refresh_jobs(&t).await;
    let bulk: Vec<i64> = jobs[3..]
        .iter()
        .map(|j| j.1["merge_request_id"].as_i64().expect("id"))
        .collect();
    assert_eq!(bulk, [f, o], "refs_truncated re-checks every open one");

    // The subscriber's dedup: the same event handled again adds nothing.
    let seq: i64 = sqlx::query_scalar(
        "SELECT seq FROM forge.events WHERE event_type = 'repository.pushed' ORDER BY seq LIMIT 1",
    )
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("seq");
    sqlx::query("UPDATE forge.event_subscribers SET last_seq = $1 WHERE name = $2")
        .bind(seq - 1)
        .bind(REFRESH_SUBSCRIBER)
        .execute(&mut t.superuser().await)
        .await
        .expect("rewind");
    drain(&t).await;
    assert_eq!(refresh_jobs(&t).await.len(), 5, "no job twice");

    // A reopen enqueues one too.
    let _closed = close_mr_decided(
        &t.db,
        &as_user(alice, "req-close"),
        p,
        other.iid,
        alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
    let _reopened = reopen_mr_decided(
        &t.db,
        &as_user(alice, "req-reopen"),
        p,
        other.iid,
        alice,
        None,
        |_| Ok(()),
    )
    .await
    .expect("reopen");
    let jobs = refresh_jobs(&t).await;
    assert!(
        jobs.last()
            .is_some_and(|j| j.0.starts_with(&format!("mr_refresh:{o}:reopened:"))),
        "{jobs:?}"
    );
    t.drop_now().await;
}

#[test]
fn a_job_payload_round_trips_and_garbage_is_refused() {
    let job = refresh_job(
        RefreshRequest {
            merge_request: MergeRequestId::new(5),
            pushed_by: Some(UserId::new(2)),
        },
        RefreshTrigger::Pushed { seq: 9 },
        RequestId::parse("req-x").expect("id"),
        Some(9),
    )
    .expect("job");
    assert_eq!(job.kind.as_str(), REFRESH_JOB);
    assert_eq!(
        job.dedup_key.as_ref().map(|k| k.as_str()),
        Some("mr_refresh:5:push:9")
    );
    let back = refresh_payload(&job.payload).expect("payload");
    assert_eq!(back.merge_request, MergeRequestId::new(5));
    assert_eq!(back.pushed_by, Some(UserId::new(2)));
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"merge_request_id": 0}),
        serde_json::json!({"merge_request_id": 1, "pushed_by": "x"}),
    ] {
        assert!(refresh_payload(&bad).is_err(), "{bad}");
    }
}

/// S0 review, carried into M3 I1b: a service account's push is a
/// `service_account` event; the version job still gets its pusher, so the
/// version it records keeps `pushed_by` (it was dropped for anything but
/// `actor_kind = 'user'`).
#[tokio::test]
async fn a_service_accounts_push_keeps_pushed_by_on_the_version() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    subscribers::register(&t.db, &MergeRequestRefresh::default(), Start::Head)
        .await
        .expect("register");
    let bot = crate::service_accounts::create(
        &t.db,
        &system("req-sa-bot"),
        &crate::service_accounts::NewServiceAccount {
            username: "release-bot".to_owned(),
            display_name: "Release bot".to_owned(),
            owner: alice,
            purpose: "Pushes generated changes".to_owned(),
        },
        alice,
    )
    .await
    .expect("service account")
    .user_id;
    let mr = open(&t, alice, web.project_id, "feature").await;
    let who = Pusher {
        request_id: RequestId::parse("req-push-bot").expect("request id"),
        user: bot,
        kind: forge_core::UserKind::ServiceAccount,
        credential_id: None,
        source_ip: None,
        user_agent: None,
    };
    let recorder: &dyn PushRecorder = &t.db;
    let done = recorder
        .record_push(
            &who,
            &PushRecord {
                repository_id: web.repository_id,
                updates: vec![pushed("refs/heads/feature", PushUpdateKind::FastForward)],
            },
        )
        .await
        .expect("push");
    assert_eq!(done, forge_core::PushRecorded::Recorded);
    drain(&t).await;
    let jobs = refresh_jobs(&t).await;
    let (key, payload) = jobs.last().expect("the push's job");
    assert!(
        key.starts_with(&format!("mr_refresh:{}:push:", mr.id.get())),
        "{key}"
    );
    assert_eq!(payload["pushed_by"], bot.get(), "{payload}");
    // What the job then records for that version.
    let pushed_by = payload["pushed_by"].as_i64().map(UserId::new);
    record_mr_version(&t.db, &system("req-v-bot"), mr.id, &oid('b'), pushed_by)
        .await
        .expect("version");
    let versions = list_versions(&t.db, mr.id).await.expect("versions");
    assert_eq!(
        versions.last().and_then(|v| v.pushed_by),
        Some(bot),
        "{versions:?}"
    );
    t.drop_now().await;
}
