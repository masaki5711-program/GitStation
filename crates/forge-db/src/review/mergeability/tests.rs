//! The mergeability status against a migrated database (M3 R2, ADR-0024
//! §3): requests and their seq, superseded runs, flips and their events,
//! retargets, target pushes with the fan-out cap, deleted targets, and the
//! bases of a version.

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{
    CheckReason, EventType, MergeabilityStatus, ObjectId, PushRecord, PushRecorder, PushUpdateKind,
    PushedRef, Pusher, RefName, RequestId,
};

use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::*;
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

fn branch(name: &str) -> RefName {
    RefName::parse(&format!("refs/heads/{name}")).expect("branch")
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

async fn open_to(
    t: &FreshDb,
    who: UserId,
    p: ProjectId,
    source: &str,
    target: &str,
) -> StoredMergeRequest {
    let new = NewMergeRequest {
        title: format!("Merge {source}"),
        description: String::new(),
        source_branch: branch(source),
        target_branch: branch(target),
        draft: false,
    };
    open_mr_decided(
        &t.db,
        &as_user(who, &format!("req-open-{source}")),
        p,
        who,
        &new,
        |_| Ok(()),
    )
    .await
    .map(|(stored, ())| stored)
    .expect("open")
}

/// [`open_to`], then version 1 at head `a` (what every result names).
async fn open_at_a(
    t: &FreshDb,
    who: UserId,
    p: ProjectId,
    source: &str,
    target: &str,
) -> StoredMergeRequest {
    let mr = open_to(t, who, p, source, target).await;
    let done = record_mr_version(
        &t.db,
        &system(&format!("req-v-{source}")),
        mr.id,
        &oid('a'),
        None,
    )
    .await
    .expect("version");
    assert_eq!(done, VersionOutcome::Added { version: 1 });
    mr
}

/// (dedup_key, payload) of the mergeability jobs, oldest first.
async fn checks(t: &FreshDb) -> Vec<(String, serde_json::Value)> {
    sqlx::query_as(
        "SELECT dedup_key, payload FROM forge.jobs WHERE kind = 'merge_request.mergeability' \
         ORDER BY id",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("jobs")
}

/// (event_type, dedup_key, payload) of the conflict events, oldest first.
async fn flips(t: &FreshDb) -> Vec<(String, String, serde_json::Value)> {
    sqlx::query_as(
        "SELECT event_type, dedup_key, payload FROM forge.events \
         WHERE event_type LIKE 'merge_request.conflict_%' ORDER BY id",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("events")
}

async fn audit_count(t: &FreshDb, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM forge.audit_log WHERE action = $1")
        .bind(action)
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count")
}

fn result(status: MergeabilityStatus, head: char, target: char) -> CheckResult {
    let (reason, conflicts) = match status {
        MergeabilityStatus::CannotBeMerged => {
            (Some(CheckReason::Conflict), vec!["README".to_owned()])
        }
        MergeabilityStatus::CheckFailed => (Some(CheckReason::GitTimeout), Vec::new()),
        MergeabilityStatus::Unchecked
        | MergeabilityStatus::Checking
        | MergeabilityStatus::CanBeMerged => (None, Vec::new()),
    };
    CheckResult {
        status,
        reason,
        head: Some(oid(head)),
        target: Some(oid(target)),
        conflicts,
        conflicts_truncated: false,
    }
}

/// One full run: a request, its start, its result.
async fn run(t: &FreshDb, mr: MergeRequestId, status: MergeabilityStatus, n: usize) -> Recorded {
    let seq = request_check(&t.db, mr, RequestId::parse("req-r").expect("id"))
        .await
        .expect("request")
        .expect("open");
    let start = begin_check(&t.db, mr, seq).await.expect("begin");
    assert!(matches!(start, CheckStart::Check(_)), "{start:?}");
    record_check(
        &t.db,
        &system(&format!("req-run-{n}")),
        mr,
        seq,
        &result(status, 'a', 'b'),
    )
    .await
    .expect("record")
}

/// ADR-0024 §3, §6: the conflict events come only on a can ⇄ cannot flip
/// — not on the first decision, not to or from an undecided status (a
/// can → failed → cannot path is still one flip) — each once, with its
/// own audit row, deduplicated per check.
#[tokio::test]
async fn conflict_events_are_emitted_once_per_flip() {
    use MergeabilityStatus::{CanBeMerged as Can, CannotBeMerged as Cannot, CheckFailed as Failed};
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let mr = open_at_a(&t, alice, web.project_id, "feature", "main").await;
    let steps = [
        (Cannot, None),
        (Cannot, None),
        (Can, Some(EventType::MergeRequestConflictResolved)),
        (Failed, None),
        (Cannot, Some(EventType::MergeRequestConflictDetected)),
        (Failed, None),
        (Can, Some(EventType::MergeRequestConflictResolved)),
        (Can, None),
    ];
    for (n, (status, flip)) in steps.into_iter().enumerate() {
        let done = run(&t, mr.id, status, n).await;
        assert_eq!(done, Recorded::Stored { flip }, "step {n}: {status:?}");
    }
    let events = flips(&t).await;
    let types: Vec<&str> = events.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(
        types,
        [
            "merge_request.conflict_resolved",
            "merge_request.conflict_detected",
            "merge_request.conflict_resolved"
        ]
    );
    let id = mr.id.get();
    let detected = &events[1];
    let seq = detected.2["check_seq"].as_i64().expect("seq");
    assert_eq!(
        detected.1,
        format!("merge_request_conflict_detected:{id}:{seq}")
    );
    assert_eq!(detected.2["head_sha"], "a".repeat(40));
    assert_eq!(detected.2["target_sha"], "b".repeat(40));
    assert_eq!(detected.2["conflict_files"], 1);
    assert!(
        detected.2.get("files").is_none(),
        "no path in the event: {}",
        detected.2
    );
    assert_eq!(audit_count(&t, "merge_request.conflict_detected").await, 1);
    assert_eq!(audit_count(&t, "merge_request.conflict_resolved").await, 2);
    let stored = mergeability_written(&t.db, mr.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(stored.status, Can);
    assert_eq!(stored.checked_head_sha, Some(oid('a')));
    assert!(stored.checked_at.is_some());
    t.drop_now().await;
}

/// ADR-0024 §3, decision 2: only the latest request's job writes; an
/// older one is superseded at its start and at its end.
#[tokio::test]
async fn an_older_check_never_overwrites_a_newer_request() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let mr = open_to(&t, alice, web.project_id, "feature", "main").await;
    let origin = || RequestId::parse("req-r").expect("id");
    let first = request_check(&t.db, mr.id, origin())
        .await
        .expect("request")
        .expect("open");
    let start = begin_check(&t.db, mr.id, first).await.expect("begin");
    let CheckStart::Check(subject) = start else {
        panic!("{start:?}");
    };
    assert_eq!(subject.target_branch, branch("main"));
    assert_eq!(subject.head, None, "no version yet");
    let second = request_check(&t.db, mr.id, origin())
        .await
        .expect("request")
        .expect("open");
    assert_eq!(second, first + 1);
    let late = record_check(
        &t.db,
        &system("req-late"),
        mr.id,
        first,
        &result(MergeabilityStatus::CanBeMerged, 'a', 'b'),
    )
    .await
    .expect("record");
    assert_eq!(late, Recorded::Superseded);
    let again = begin_check(&t.db, mr.id, first).await.expect("begin");
    assert_eq!(
        again,
        CheckStart::Superseded {
            latest: Some(second)
        }
    );
    let stored = mergeability_written(&t.db, mr.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(stored.status, MergeabilityStatus::Unchecked);
    assert_eq!(stored.check_seq, second);
    let keys: Vec<String> = checks(&t).await.into_iter().map(|c| c.0).collect();
    let id = mr.id.get();
    assert_eq!(
        keys,
        [
            format!("mr_mergeability:{id}:{first}"),
            format!("mr_mergeability:{id}:{second}")
        ]
    );
    t.drop_now().await;
}

/// A retarget resets the status to `unchecked` and enqueues a check in
/// the edit's own transaction; an edit that does not retarget does not.
#[tokio::test]
async fn a_retarget_resets_to_unchecked_and_enqueues_a_check() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let mr = open_at_a(&t, alice, p, "feature", "main").await;
    run(&t, mr.id, MergeabilityStatus::CanBeMerged, 0).await;
    let before = checks(&t).await.len();
    let title = MergeRequestPatch {
        title: Some("Renamed".to_owned()),
        ..MergeRequestPatch::default()
    };
    let _done = edit_mr_decided(
        &t.db,
        &as_user(alice, "req-title"),
        p,
        mr.iid,
        alice,
        &title,
        |_| Ok(()),
    )
    .await
    .expect("edit");
    assert_eq!(
        checks(&t).await.len(),
        before,
        "a title edit checks nothing"
    );
    let retarget = MergeRequestPatch {
        target_branch: Some(branch("release")),
        ..MergeRequestPatch::default()
    };
    let _done = edit_mr_decided(
        &t.db,
        &as_user(alice, "req-retarget"),
        p,
        mr.iid,
        alice,
        &retarget,
        |_| Ok(()),
    )
    .await
    .expect("retarget");
    let stored = mergeability_written(&t.db, mr.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(stored.status, MergeabilityStatus::Unchecked);
    let jobs = checks(&t).await;
    assert_eq!(jobs.len(), before + 1);
    let (key, payload) = jobs.last().expect("job");
    assert_eq!(
        key,
        &format!("mr_mergeability:{}:{}", mr.id.get(), stored.check_seq)
    );
    assert_eq!(payload["check_seq"], stored.check_seq);
    // The next run checks against the new target.
    let start = begin_check(&t.db, mr.id, stored.check_seq)
        .await
        .expect("begin");
    let CheckStart::Check(subject) = start else {
        panic!("{start:?}");
    };
    assert_eq!(subject.target_branch, branch("release"));
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
        credential_id: None,
        source_ip: None,
        user_agent: None,
    };
    let recorder: &dyn PushRecorder = &t.db;
    let done = recorder.record_push(&who, push).await.expect("push");
    assert_eq!(done, forge_core::PushRecorded::Recorded);
}

async fn drain(t: &FreshDb, subscriber: &MergeRequestRefresh) {
    crate::sequencer::run_once(
        &t.db,
        crate::sequencer::BatchSize::MAX,
        crate::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
    loop {
        match subscribers::run_once(&t.db, subscriber)
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

/// ADR-0024 §3: a push to a target checks the open merge requests
/// targeting it, the most recently updated first, at most the fan-out cap;
/// the rest are `unchecked` (a new seq, no job). Closed ones and other
/// targets are left alone. Deleting the target checks again (the job then
/// stores `target_missing`).
#[tokio::test]
async fn a_target_push_checks_open_merge_requests_up_to_the_fanout_cap() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let subscriber = MergeRequestRefresh::new(2);
    subscribers::register(&t.db, &subscriber, Start::Head)
        .await
        .expect("register");
    let a = open_to(&t, alice, p, "a", "main").await;
    let b = open_to(&t, alice, p, "b", "main").await;
    let c = open_to(&t, alice, p, "c", "main").await;
    let other = open_to(&t, alice, p, "d", "release").await;
    let closed = open_to(&t, alice, p, "e", "main").await;
    let _done = close_mr_decided(
        &t.db,
        &as_user(alice, "req-close"),
        p,
        closed.iid,
        alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
    // `a` was touched last: it is among the two that are checked.
    let _done = edit_mr_decided(
        &t.db,
        &as_user(alice, "req-touch"),
        p,
        a.iid,
        alice,
        &MergeRequestPatch {
            title: Some("Touched".to_owned()),
            ..MergeRequestPatch::default()
        },
        |_| Ok(()),
    )
    .await
    .expect("touch");
    push(
        &t,
        alice,
        "req-push-main",
        &PushRecord {
            repository_id: web.repository_id,
            updates: vec![pushed("refs/heads/main", PushUpdateKind::FastForward)],
        },
    )
    .await;
    drain(&t, &subscriber).await;
    let checked: Vec<i64> = checks(&t)
        .await
        .iter()
        .map(|j| j.1["merge_request_id"].as_i64().expect("id"))
        .collect();
    assert_eq!(
        checked,
        [a.id.get(), c.id.get()],
        "the two most recently updated"
    );
    let deferred = mergeability_written(&t.db, b.id)
        .await
        .expect("read")
        .expect("deferred row");
    assert_eq!(deferred.status, MergeabilityStatus::Unchecked);
    for untouched in [other.id, closed.id] {
        assert_eq!(
            mergeability_written(&t.db, untouched).await.expect("read"),
            None
        );
    }
    // The target is deleted: checked again (up to the cap).
    push(
        &t,
        alice,
        "req-delete-main",
        &PushRecord {
            repository_id: web.repository_id,
            updates: vec![pushed("refs/heads/main", PushUpdateKind::Delete)],
        },
    )
    .await;
    drain(&t, &subscriber).await;
    assert_eq!(checks(&t).await.len(), 4);
    let b_after = mergeability_written(&t.db, b.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(b_after.check_seq, deferred.check_seq + 1, "deferred again");
    t.drop_now().await;
}

/// ADR-0024 §1, R2 decision 4: a version's bases are set once, together;
/// an older version keeps its bases when the target moves.
#[tokio::test]
async fn version_bases_are_recorded_once_with_an_audit_row() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let mr = open_to(&t, alice, web.project_id, "feature", "main").await;
    let ctx = system("req-v");
    record_mr_version(&t.db, &ctx, mr.id, &oid('1'), None)
        .await
        .expect("v1");
    assert!(
        record_version_bases(&t.db, &ctx, mr.id, 1, &oid('b'), Some(&oid('c')))
            .await
            .expect("bases")
    );
    record_mr_version(&t.db, &ctx, mr.id, &oid('2'), None)
        .await
        .expect("v2");
    assert!(record_version_bases(&t.db, &ctx, mr.id, 2, &oid('d'), None)
        .await
        .expect("bases v2"));
    // The target moved: v1 keeps what it had.
    assert!(
        !record_version_bases(&t.db, &ctx, mr.id, 1, &oid('e'), Some(&oid('e')))
            .await
            .expect("again")
    );
    assert!(
        !record_version_bases(&t.db, &ctx, mr.id, 9, &oid('e'), None)
            .await
            .expect("no such version")
    );
    let versions = read_versions(&t.db, mr.id).await.expect("versions");
    assert_eq!(versions[0].base_sha, Some(oid('b')));
    assert_eq!(versions[0].merge_base_sha, Some(oid('c')));
    assert_eq!(versions[1].base_sha, Some(oid('d')));
    assert_eq!(versions[1].merge_base_sha, None);
    assert_eq!(
        audit_count(&t, "merge_request.version_bases_recorded").await,
        2
    );
    t.drop_now().await;
}

#[test]
fn a_mergeability_payload_round_trips_and_garbage_is_refused() {
    let job = mergeability_job(
        MergeRequestId::new(5),
        3,
        RequestId::parse("req-x").expect("id"),
        None,
    )
    .expect("job");
    assert_eq!(job.kind.as_str(), MERGEABILITY_JOB);
    assert_eq!(
        job.dedup_key.as_ref().map(|k| k.as_str()),
        Some("mr_mergeability:5:3")
    );
    let back = mergeability_payload(&job.payload).expect("payload");
    assert_eq!(
        back,
        MergeabilityRequest {
            merge_request: MergeRequestId::new(5),
            check_seq: 3
        }
    );
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"merge_request_id": 1}),
        serde_json::json!({"merge_request_id": 1, "check_seq": 0}),
    ] {
        assert!(mergeability_payload(&bad).is_err(), "{bad}");
    }
}

/// Start a run: a request and its `begin_check` (status `checking`).
async fn checking(t: &FreshDb, mr: MergeRequestId) -> i64 {
    let seq = request_check(&t.db, mr, RequestId::parse("req-c").expect("id"))
        .await
        .expect("request")
        .expect("open");
    let start = begin_check(&t.db, mr, seq).await.expect("begin");
    assert!(matches!(start, CheckStart::Check(_)), "{start:?}");
    seq
}

/// The jobs of `mr` are gone for good (as after a dead letter).
async fn kill_jobs(t: &FreshDb, mr: MergeRequestId) {
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET state = 'dead', finished_at = now(), \
         last_error_code = 'FG-DB-0007' \
         WHERE kind = 'merge_request.mergeability' \
         AND dedup_key LIKE 'mr_mergeability:{}:%'",
        mr.get()
    ))
    .await;
}

/// Review (silent-failure HIGH): a row left `checking` — the job hit its
/// time limit and died, or the worker crashed — is ended as `check_failed
/// reason=job_stale` once no live job holds it, logged with FG-REVIEW-0003;
/// doctor's counts see it first. A live job keeps the row.
#[tokio::test]
async fn a_stale_checking_row_is_reaped_as_job_stale() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let mr = open_at_a(&t, alice, web.project_id, "feature", "main").await;
    let seq = checking(&t, mr.id).await;
    let none = reap_stale_checks(&t.db, std::time::Duration::ZERO, 10)
        .await
        .expect("reap");
    assert!(none.is_empty(), "a queued job still holds it");
    kill_jobs(&t, mr.id).await;
    t.exec_superuser(
        "UPDATE forge.merge_request_mergeability SET updated_at = now() - interval '1 hour'",
    )
    .await;
    let health = mergeability_health(&t.db)
        .await
        .expect("health")
        .expect("migrated");
    assert_eq!(health.checking_stale, 1);
    let err = health.check().expect_err("stale");
    assert_eq!(err.code(), forge_errors::ErrorCode::MergeabilityNotChecked);
    let reaped = reap_stale_checks(&t.db, STALE_AFTER, 10)
        .await
        .expect("reap");
    assert_eq!(reaped, [(mr.id, seq)]);
    let stored = mergeability_written(&t.db, mr.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(stored.status, MergeabilityStatus::CheckFailed);
    assert_eq!(stored.reason, Some(CheckReason::JobStale));
    let health = mergeability_health(&t.db)
        .await
        .expect("health")
        .expect("migrated");
    health.check().expect("nothing left undecided");
    t.drop_now().await;
}

/// A run that failed for good (dead letter, permanent failure) ends its own
/// request as `job_failed` — never an older request's, never a decision.
#[tokio::test]
async fn a_job_that_failed_for_good_leaves_check_failed_job_failed() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let mr = open_at_a(&t, alice, web.project_id, "feature", "main").await;
    let old = checking(&t, mr.id).await;
    let seq = checking(&t, mr.id).await;
    assert!(
        !fail_check(&t.db, mr.id, old).await.expect("old"),
        "superseded"
    );
    assert!(fail_check(&t.db, mr.id, seq).await.expect("fail"));
    let stored = mergeability_written(&t.db, mr.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        (stored.status, stored.reason),
        (
            MergeabilityStatus::CheckFailed,
            Some(CheckReason::JobFailed)
        )
    );
    run(&t, mr.id, MergeabilityStatus::CanBeMerged, 1).await;
    let decided = mergeability_written(&t.db, mr.id)
        .await
        .expect("read")
        .expect("row");
    assert!(!fail_check(&t.db, mr.id, decided.check_seq)
        .await
        .expect("decided"));
    t.drop_now().await;
}

/// Review (fan-out starvation): the sweep re-requests open merge requests
/// `unchecked` with no live job, the longest waiting first, a batch at a
/// time; closed ones and rows with a live job are left alone.
#[tokio::test]
async fn the_sweep_requests_unchecked_rows_without_a_live_job_oldest_first() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let first = open_to(&t, alice, p, "a", "main").await;
    let second = open_to(&t, alice, p, "b", "main").await;
    let live = open_to(&t, alice, p, "c", "main").await;
    let closed = open_to(&t, alice, p, "d", "main").await;
    let origin = || RequestId::parse("req-r").expect("id");
    for mr in [&first, &second, &live, &closed] {
        request_check(&t.db, mr.id, origin())
            .await
            .expect("request")
            .expect("open");
    }
    for mr in [&first, &second, &closed] {
        kill_jobs(&t, mr.id).await;
    }
    t.exec_superuser(&format!(
        "UPDATE forge.merge_request_mergeability SET updated_at = now() - interval '2 hours' \
         WHERE merge_request_id = {}",
        first.id.get()
    ))
    .await;
    let _closed = close_mr_decided(
        &t.db,
        &as_user(alice, "req-close"),
        p,
        closed.iid,
        alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
    let sweep = RequestId::parse("req-sweep").expect("id");
    let one = sweep_unchecked(&t.db, 1, &sweep).await.expect("sweep");
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].0, first.id, "the longest waiting first");
    let two = sweep_unchecked(&t.db, 10, &sweep).await.expect("sweep");
    let swept: Vec<MergeRequestId> = two.iter().map(|(m, _)| *m).collect();
    assert_eq!(swept, [second.id], "live and closed ones are not swept");
    let three = sweep_unchecked(&t.db, 10, &sweep).await.expect("sweep");
    assert!(three.is_empty(), "every swept row has a live job now");
    t.drop_now().await;
}

/// Review (stale decisions): a new version marks a decided status
/// `unchecked` in its own transaction; a result for a head that moved
/// since the run started, or for a merge request closed meanwhile, is not
/// stored.
#[tokio::test]
async fn a_new_head_downgrades_and_a_result_for_an_old_head_is_superseded() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let mr = open_at_a(&t, alice, p, "feature", "main").await;
    run(&t, mr.id, MergeabilityStatus::CanBeMerged, 0).await;
    let seq = checking(&t, mr.id).await;
    record_mr_version(&t.db, &system("req-v2"), mr.id, &oid('b'), None)
        .await
        .expect("v2");
    let late = record_check(
        &t.db,
        &system("req-late"),
        mr.id,
        seq,
        &result(MergeabilityStatus::CanBeMerged, 'a', 'c'),
    )
    .await
    .expect("record");
    assert_eq!(late, Recorded::Superseded, "the head moved");
    // A decided status for `b`, then version `c`: downgraded at once.
    let seq = checking(&t, mr.id).await;
    let ok = result(MergeabilityStatus::CanBeMerged, 'b', 'c');
    let stored = record_check(&t.db, &system("req-b"), mr.id, seq, &ok)
        .await
        .expect("record");
    assert_eq!(stored, Recorded::Stored { flip: None });
    record_mr_version(&t.db, &system("req-v3"), mr.id, &oid('c'), None)
        .await
        .expect("v3");
    let now = mergeability_written(&t.db, mr.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(now.status, MergeabilityStatus::Unchecked);
    // Closed while checking: the result is not stored.
    let seq = checking(&t, mr.id).await;
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
    let c = result(MergeabilityStatus::CanBeMerged, 'c', 'd');
    let closed = record_check(&t.db, &system("req-c-late"), mr.id, seq, &c)
        .await
        .expect("record");
    assert_eq!(closed, Recorded::Superseded);
    t.drop_now().await;
}

/// Review (LOW): purging the version a stored check names resets the
/// status and empties its conflict list (nobody may read that commit).
#[tokio::test]
async fn purging_the_checked_version_resets_the_mergeability() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let mr = open_at_a(&t, alice, p, "feature", "main").await;
    run(&t, mr.id, MergeabilityStatus::CannotBeMerged, 0).await;
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
        Some(&[1]),
        |_| Ok(()),
    )
    .await
    .expect("purge");
    let stored = mergeability_written(&t.db, mr.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(stored.status, MergeabilityStatus::Unchecked);
    assert!(stored.conflicts.is_empty());
    t.drop_now().await;
}

/// Two runs recording the same version's bases at once: one writes.
#[tokio::test]
async fn concurrent_bases_writes_record_once() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let mr = open_at_a(&t, alice, web.project_id, "feature", "main").await;
    let (c1, c2) = (system("req-b1"), system("req-b2"));
    let (b1, b2) = (oid('b'), oid('c'));
    let (one, two) = tokio::join!(
        record_version_bases(&t.db, &c1, mr.id, 1, &b1, Some(&b1)),
        record_version_bases(&t.db, &c2, mr.id, 1, &b2, None),
    );
    let written = [one.expect("one"), two.expect("two")];
    assert_eq!(written.iter().filter(|w| **w).count(), 1, "{written:?}");
    t.drop_now().await;
}
