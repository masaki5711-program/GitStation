//! CI pipelines against a migrated database (M4 C2): the job payload, the
//! `ci-pipelines` subscriber (one job per pushed branch and per merge
//! request version, none for a deleted branch, nothing new on
//! redelivery), the idempotent write, the `invalid` row, and the cancel /
//! retry transitions.

use forge_core::{
    MergeRequestId, ObjectId, ProjectId, PushRecord, PushRecorder, PushUpdateKind, PushedRef,
    Pusher, RefName, RequestId, UserId,
};
use forge_errors::{ErrorCode, ForgeError};

use super::subscriber::{PipelineRequests, PIPELINES_SUBSCRIBER};
use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::{open_mr_decided, record_mr_version, NewMergeRequest};
use crate::subscribers::{self, Progress, Start, Subscriber};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn sha(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("sha")
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

async fn drain(t: &FreshDb, subscriber: &dyn Subscriber) {
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

fn pushed(name: &str, kind: PushUpdateKind, new: char) -> PushedRef {
    let zero = ObjectId::parse(&"0".repeat(40)).expect("zero");
    let (old, new) = match kind {
        PushUpdateKind::Create => (zero, sha(new)),
        PushUpdateKind::Delete => (sha('a'), zero),
        PushUpdateKind::FastForward | PushUpdateKind::Force => (sha('a'), sha(new)),
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

async fn push(t: &FreshDb, user: UserId, request: &str, record: &PushRecord) {
    let who = Pusher {
        request_id: RequestId::parse(request).expect("request id"),
        user,
        kind: forge_core::UserKind::Human,
        credential_id: None,
        source_ip: None,
        user_agent: None,
    };
    let recorder: &dyn PushRecorder = &t.db;
    let done = recorder.record_push(&who, record).await.expect("push");
    assert_eq!(done, forge_core::PushRecorded::Recorded);
}

async fn create_jobs(t: &FreshDb) -> Vec<PipelineRequest> {
    let rows: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT payload FROM forge.jobs WHERE kind = $1 ORDER BY id")
            .bind(PIPELINE_CREATE_JOB)
            .fetch_all(&mut t.superuser().await)
            .await
            .expect("jobs");
    rows.iter()
        .map(|p| create_payload(p).expect("payload"))
        .collect()
}

#[test]
fn the_payload_round_trips_and_refuses_anything_else() {
    let request = PipelineRequest {
        project: ProjectId::new(7),
        git_ref: branch("main"),
        sha: sha('d'),
        source: Source::MergeRequest,
        merge_request: Some(MergeRequestId::new(3)),
        triggered_by: Some(UserId::new(9)),
    };
    let job = create_job(&request, RequestId::generate(), Some(4)).expect("job");
    assert_eq!(job.kind.as_str(), "ci.pipeline_create");
    assert_eq!(create_payload(&job.payload).expect("parse"), request);
    let key = job.dedup_key.expect("key");
    assert!(key.as_str().starts_with("ci_pipeline:7:merge_request:3:"));
    // A very long branch still fits the key.
    let long = PipelineRequest {
        git_ref: branch(&"b".repeat(240)),
        ..request.clone()
    };
    assert!(create_dedup_key(&long).len() <= crate::jobs::DedupKey::MAX_LEN);
    for bad in [
        serde_json::json!({}),
        // A merge request pipeline without its merge request.
        serde_json::json!({"project_id": 7, "ref": "refs/heads/main", "sha": sha('d').as_str(),
            "source": "merge_request", "merge_request_id": null, "triggered_by": null}),
        // A tag is no branch.
        serde_json::json!({"project_id": 7, "ref": "refs/tags/v1", "sha": sha('d').as_str(),
            "source": "push", "merge_request_id": null, "triggered_by": null}),
        serde_json::json!({"project_id": 7, "ref": "refs/heads/main", "sha": "HEAD",
            "source": "push", "merge_request_id": null, "triggered_by": null}),
    ] {
        let err = create_payload(&bad).expect_err("bad payload");
        assert_reason(&err, ErrorCode::Internal, "invalid_job_payload");
    }
}

/// One push → one job per created or moved branch (none for a delete, none
/// for a tag); the same events again (a redelivery: the cursor moved
/// back) enqueue nothing new.
#[tokio::test]
async fn one_push_enqueues_one_job_per_branch_and_a_redelivery_none() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let subscriber = PipelineRequests;
    assert_eq!(subscriber.name(), PIPELINES_SUBSCRIBER);
    subscribers::register(&t.db, &subscriber, Start::Head)
        .await
        .expect("register");
    push(
        &t,
        alice,
        "req-push-1",
        &PushRecord {
            repository_id: web.repository_id,
            updates: vec![
                pushed("refs/heads/main", PushUpdateKind::FastForward, 'b'),
                pushed("refs/heads/feature", PushUpdateKind::Create, 'c'),
                pushed("refs/heads/old", PushUpdateKind::Delete, 'a'),
                pushed("refs/tags/v1", PushUpdateKind::Create, 'c'),
            ],
        },
    )
    .await;
    drain(&t, &subscriber).await;
    let jobs = create_jobs(&t).await;
    let mut seen: Vec<(String, String)> = jobs
        .iter()
        .map(|r| {
            assert_eq!(r.project, web.project_id);
            assert_eq!(r.source, Source::Push);
            assert_eq!(r.triggered_by, Some(alice));
            (r.git_ref.as_str().to_owned(), r.sha.as_str().to_owned())
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            (
                "refs/heads/feature".to_owned(),
                sha('c').as_str().to_owned()
            ),
            ("refs/heads/main".to_owned(), sha('b').as_str().to_owned()),
        ]
    );
    // Redelivery: the subscriber's cursor back to before the push.
    let _ = sqlx::query(
        "UPDATE forge.event_subscribers SET last_seq = 0, last_txid = NULL WHERE name = $1",
    )
    .bind(PIPELINES_SUBSCRIBER)
    .execute(&mut t.superuser().await)
    .await
    .expect("rewind");
    drain(&t, &subscriber).await;
    assert_eq!(
        create_jobs(&t).await.len(),
        2,
        "a redelivery enqueues nothing new"
    );
    t.drop_now().await;
}

/// M4 C7: a merge request version asks for no pipeline of its head (the
/// push pipeline covers the branch); the merge request pipeline runs on the
/// merge result, enqueued with its `merge_result_refs` row — once per
/// (head, target tip): the same pair again writes nothing and enqueues
/// nothing, a new target tip is a new row and a new pipeline.
#[tokio::test]
async fn a_merge_request_pipeline_runs_on_each_merge_result_once() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let subscriber = PipelineRequests;
    subscribers::register(&t.db, &subscriber, Start::Head)
        .await
        .expect("register");
    let (mr, ()) = open_mr_decided(
        &t.db,
        &ChangeContext::new(
            Actor::User(alice),
            RequestId::parse("req-open").expect("id"),
        ),
        p,
        alice,
        &NewMergeRequest {
            title: "Feature".to_owned(),
            description: String::new(),
            source_branch: branch("feature"),
            target_branch: branch("main"),
            draft: false,
        },
        |_| Ok(()),
    )
    .await
    .expect("open");
    record_mr_version(&t.db, &system("req-v1"), mr.id, &sha('e'), Some(alice))
        .await
        .expect("version");
    drain(&t, &subscriber).await;
    assert_eq!(create_jobs(&t).await, [], "no pipeline of the head");
    let origin = RequestId::parse("req-mr").expect("id");
    let first = crate::review::record_merge_result(
        &t.db,
        &origin,
        mr.id,
        (&sha('e'), &sha('1')),
        &sha('a'),
    )
    .await
    .expect("record")
    .expect("open");
    assert!(first.created && first.pipeline_enqueued);
    // The same pair again (a retried or repeated check with another commit):
    // the first result stays, nothing is enqueued.
    let again = crate::review::record_merge_result(
        &t.db,
        &origin,
        mr.id,
        (&sha('e'), &sha('1')),
        &sha('b'),
    )
    .await
    .expect("record")
    .expect("open");
    assert_eq!(
        (again.result.clone(), again.created, again.pipeline_enqueued),
        (sha('a'), false, false)
    );
    assert_eq!(
        crate::review::merge_result_of(&t.db, mr.id, &sha('e'), &sha('1'))
            .await
            .expect("read"),
        Some(sha('a'))
    );
    // The target advanced: a new merge result and its own pipeline.
    let moved = crate::review::record_merge_result(
        &t.db,
        &origin,
        mr.id,
        (&sha('e'), &sha('2')),
        &sha('c'),
    )
    .await
    .expect("record")
    .expect("open");
    assert!(moved.created && moved.pipeline_enqueued);
    let pipeline = |result: char| PipelineRequest {
        project: p,
        git_ref: branch("feature"),
        sha: sha(result),
        source: Source::MergeRequest,
        merge_request: Some(mr.id),
        triggered_by: Some(alice),
    };
    let mut jobs = create_jobs(&t).await;
    jobs.sort_by(|a, b| a.sha.as_str().cmp(b.sha.as_str()));
    assert_eq!(jobs, [pipeline('a'), pipeline('c')]);
    let rows = crate::review::merge_results(&t.db, mr.id)
        .await
        .expect("rows");
    assert_eq!(rows.len(), 2, "one row per (head, target) pair");
    t.drop_now().await;
}

fn a_job(name: &str, needs: &[&str]) -> NewCiJob {
    NewCiJob {
        name: name.to_owned(),
        stage: "test".to_owned(),
        wave: i32::from(!needs.is_empty()),
        needs: needs.iter().map(|n| (*n).to_owned()).collect(),
        image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
        timeout_s: 600,
        tags: vec!["linux".to_owned()],
        secrets: Vec::new(),
    }
}

fn new_pipeline(project: ProjectId, source: Source, planned: Planned) -> NewPipeline {
    NewPipeline {
        ai_eval_job: None,
        request: PipelineRequest {
            project,
            git_ref: branch("main"),
            sha: sha('b'),
            source,
            merge_request: None,
            triggered_by: None,
        },
        request_id: "req-plan".to_owned(),
        definition_hash: (planned != Planned::Error).then(|| definition_hash(b"version: 1")),
        protected: false,
        copy_mark: CopyMark::Open,
        planned,
    }
}

/// The same push pipeline written twice is one row (a retried job);
/// manual runs are each their own.
#[tokio::test]
async fn one_push_is_exactly_one_pipeline_and_manual_runs_are_not_deduplicated() {
    let t = FreshDb::migrated().await;
    let (_, web) = web(&t).await;
    let p = web.project_id;
    let new = new_pipeline(
        p,
        Source::Push,
        Planned::Jobs(vec![a_job("build", &[]), a_job("test", &["build"])]),
    );
    let first = create(&t.db, &new).await.expect("first");
    assert!(!first.existing);
    let again = create(&t.db, &new).await.expect("again");
    assert_eq!(
        again,
        Created {
            pipeline_id: first.pipeline_id,
            existing: true
        }
    );
    let (stored, jobs) = get(&t.db, p, first.pipeline_id)
        .await
        .expect("get")
        .expect("found");
    assert_eq!(stored.status, PipelineStatus::Pending);
    assert_eq!(stored.copy_mark, CopyMark::Open);
    let states: Vec<(&str, JobStatus, i32)> = jobs
        .iter()
        .map(|j| (j.name.as_str(), j.status, j.attempt))
        .collect();
    assert_eq!(
        states,
        [
            ("build", JobStatus::Pending, 1),
            ("test", JobStatus::Created, 1)
        ]
    );
    let manual = new_pipeline(p, Source::Manual, Planned::Jobs(vec![a_job("build", &[])]));
    let m1 = create(&t.db, &manual).await.expect("m1");
    let m2 = create(&t.db, &manual).await.expect("m2");
    assert!(!m1.existing && !m2.existing && m1.pipeline_id != m2.pipeline_id);
    assert_eq!(list(&t.db, p, None, None, 10).await.expect("list").len(), 3);
    assert_eq!(
        list(&t.db, p, Some("refs/heads/other"), None, 10)
            .await
            .expect("list")
            .len(),
        0
    );
    // Another project sees none of them.
    assert!(
        get(&t.db, ProjectId::new(p.get() + 1000), first.pipeline_id)
            .await
            .expect("get")
            .is_none()
    );
    t.drop_now().await;
}

/// An invalid definition is a row (`invalid`, FG-CI-0001, the problems),
/// never dropped; nothing to read is `error` (FG-CI-0002); no job selected
/// is `skipped`.
#[tokio::test]
async fn invalid_error_and_skipped_pipelines_are_rows_with_their_reason() {
    let t = FreshDb::migrated().await;
    let (_, web) = web(&t).await;
    let p = web.project_id;
    let invalid = create(
        &t.db,
        &new_pipeline(
            p,
            Source::Push,
            Planned::Invalid(vec![StoredProblem {
                line: 7,
                reason: "secret_on_unprotected_ref".to_owned(),
                subject: Some("deploy".to_owned()),
            }]),
        ),
    )
    .await
    .expect("invalid");
    let (stored, jobs) = get(&t.db, p, invalid.pipeline_id)
        .await
        .expect("get")
        .expect("found");
    assert_eq!(stored.status, PipelineStatus::Invalid);
    assert_eq!(stored.error_code.as_deref(), Some("FG-CI-0001"));
    assert_eq!(stored.problems[0].reason, "secret_on_unprotected_ref");
    assert_eq!(stored.problems[0].line, 7);
    assert!(jobs.is_empty());
    let mut err = new_pipeline(p, Source::Manual, Planned::Error);
    err.request.git_ref = branch("other");
    let error = create(&t.db, &err).await.expect("error");
    let (stored, _) = get(&t.db, p, error.pipeline_id)
        .await
        .expect("get")
        .expect("found");
    assert_eq!(stored.status, PipelineStatus::Error);
    assert_eq!(stored.error_code.as_deref(), Some("FG-CI-0002"));
    assert_eq!(stored.definition_hash, None);
    let mut skip = new_pipeline(p, Source::Manual, Planned::Jobs(Vec::new()));
    skip.request.git_ref = branch("docs");
    let skipped = create(&t.db, &skip).await.expect("skipped");
    let (stored, _) = get(&t.db, p, skipped.pipeline_id)
        .await
        .expect("get")
        .expect("found");
    assert_eq!(stored.status, PipelineStatus::Skipped);
    t.drop_now().await;
}

async fn force_status(t: &FreshDb, pipeline: i64, status: &str, jobs: &str) {
    let mut su = t.superuser().await;
    let _ = sqlx::query("UPDATE forge.ci_pipelines SET status = $2 WHERE id = $1")
        .bind(pipeline)
        .bind(status)
        .execute(&mut su)
        .await
        .expect("pipeline");
    let _ = sqlx::query("UPDATE forge.ci_jobs SET status = $2 WHERE pipeline_id = $1")
        .bind(pipeline)
        .bind(jobs)
        .execute(&mut su)
        .await
        .expect("jobs");
}

fn by(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
}

/// The audit rows of pipeline changes: action, actor kind, actor, details.
async fn pipeline_audit(t: &FreshDb) -> Vec<(String, String, Option<i64>, serde_json::Value)> {
    sqlx::query_as(
        "SELECT action, actor_kind, actor_user_id, details FROM forge.audit_log \
         WHERE action LIKE 'pipeline.%' AND outcome = 'succeeded' ORDER BY id",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("audit")
}

/// Cancel: pending → canceled (jobs too), one `pipeline.canceled` row with
/// the person; a finished pipeline is FG-CI-0003 and writes no row.
/// Retry: canceled / failed → pending with a new attempt
/// (`pipeline.retried`); a pending one is FG-CI-0003.
#[tokio::test]
async fn cancelling_a_finished_pipeline_is_a_state_conflict() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let created = create(
        &t.db,
        &new_pipeline(
            p,
            Source::Push,
            Planned::Jobs(vec![a_job("build", &[]), a_job("test", &["build"])]),
        ),
    )
    .await
    .expect("create");
    let id = created.pipeline_id;
    let user = |r: &str| by(Actor::User(alice), r);
    cancel_pipeline_decided(&t.db, &user("req-cancel"), (p, id), alice, |_| Ok(()))
        .await
        .expect("cancel");
    let (stored, jobs) = get(&t.db, p, id).await.expect("get").expect("found");
    assert_eq!(stored.status, PipelineStatus::Canceled);
    assert!(stored.finished_at.is_some());
    assert!(jobs.iter().all(|j| j.status == JobStatus::Canceled));
    let again = cancel_pipeline_decided(&t.db, &user("req-cancel-2"), (p, id), alice, |_| Ok(()))
        .await
        .expect_err("finished");
    assert_eq!(again.code(), ErrorCode::PipelineStateConflict);
    assert_reason_ctx(&again, "status", "canceled");

    retry_pipeline_decided(&t.db, &user("req-retry"), (p, id), alice, |_| Ok(()))
        .await
        .expect("retry");
    let (stored, jobs) = get(&t.db, p, id).await.expect("get").expect("found");
    assert_eq!(stored.status, PipelineStatus::Pending);
    assert_eq!(stored.finished_at, None);
    let states: Vec<(JobStatus, i32)> = jobs.iter().map(|j| (j.status, j.attempt)).collect();
    assert_eq!(
        states,
        [(JobStatus::Pending, 2), (JobStatus::Created, 2)],
        "a new attempt, ready by its needs"
    );
    let pending = retry_pipeline_decided(&t.db, &user("req-retry-2"), (p, id), alice, |_| Ok(()))
        .await
        .expect_err("pending");
    assert_eq!(pending.code(), ErrorCode::PipelineStateConflict);

    // A failed pipeline retries its failed jobs only.
    force_status(&t, id, "failed", "succeeded").await;
    let _ = sqlx::query(
        "UPDATE forge.ci_jobs SET status = 'failed' WHERE pipeline_id = $1 AND name = 'test'",
    )
    .bind(id)
    .execute(&mut t.superuser().await)
    .await
    .expect("one failed");
    retry_pipeline_decided(&t.db, &user("req-retry-3"), (p, id), alice, |_| Ok(()))
        .await
        .expect("retry failed");
    let (_, jobs) = get(&t.db, p, id).await.expect("get").expect("found");
    let states: Vec<(&str, JobStatus, i32)> = jobs
        .iter()
        .map(|j| (j.name.as_str(), j.status, j.attempt))
        .collect();
    assert_eq!(
        states,
        [
            ("build", JobStatus::Succeeded, 2),
            ("test", JobStatus::Created, 3)
        ]
    );
    // A refused decision changes nothing and writes no succeeded row.
    let denied = cancel_pipeline_decided(&t.db, &user("req-denied"), (p, id), alice, |_| {
        Err::<(), _>(ForgeError::new(ErrorCode::Forbidden))
    })
    .await
    .expect_err("denied");
    assert_eq!(denied.code(), ErrorCode::Forbidden);
    // No such pipeline in the project.
    let missing =
        cancel_pipeline_decided(&t.db, &user("req-missing"), (p, id + 1000), alice, |_| {
            Ok(())
        })
        .await
        .expect_err("missing");
    assert_eq!(missing.code(), ErrorCode::NotFound);
    // One succeeded row per change, with the person and the counts.
    let rows = pipeline_audit(&t).await;
    let actions: Vec<(&str, &str, Option<i64>)> = rows
        .iter()
        .map(|(a, k, u, _)| (a.as_str(), k.as_str(), *u))
        .collect();
    assert_eq!(
        actions,
        [
            ("pipeline.canceled", "user", Some(alice.get())),
            ("pipeline.retried", "user", Some(alice.get())),
            ("pipeline.retried", "user", Some(alice.get())),
        ]
    );
    assert_eq!(
        rows[0].3,
        serde_json::json!({"project_id": p.get(), "pipeline_id": id, "jobs": 2})
    );
    assert_eq!(rows[2].3["jobs"], 1);
    t.drop_now().await;
}

/// A manual run by a service account: the pipeline and one
/// `pipeline.run_manual` row naming the service account, in one
/// transaction; a push pipeline is refused by the manual command.
#[tokio::test]
async fn a_manual_run_writes_its_audit_row_with_the_actor() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let sa = crate::service_accounts::create(
        &t.db,
        &system("req-sa"),
        &crate::service_accounts::NewServiceAccount {
            username: "kanflow-bot".to_owned(),
            display_name: "Kanflow".to_owned(),
            owner: alice,
            purpose: "synthetic manual runs".to_owned(),
        },
        alice,
    )
    .await
    .expect("sa")
    .user_id;
    let mut manual = new_pipeline(p, Source::Manual, Planned::Jobs(vec![a_job("build", &[])]));
    manual.request.triggered_by = Some(sa);
    let ctx = by(Actor::ServiceAccount(sa), "req-manual");
    let (id, ()) = run_manual_decided(&t.db, &ctx, &manual, sa, |_| Ok(()))
        .await
        .expect("manual");
    let (stored, jobs) = get(&t.db, p, id).await.expect("get").expect("found");
    assert_eq!(stored.source, Source::Manual);
    assert_eq!(stored.triggered_by, Some(sa));
    assert_eq!(jobs.len(), 1);
    let rows = pipeline_audit(&t).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "pipeline.run_manual");
    assert_eq!(rows[0].1, "service_account");
    assert_eq!(rows[0].2, Some(sa.get()));
    assert_eq!(rows[0].3["pipeline_id"], id);
    assert_eq!(rows[0].3["status"], "pending");
    let push = new_pipeline(p, Source::Push, Planned::Jobs(Vec::new()));
    let err = run_manual_decided(&t.db, &ctx, &push, sa, |_| Ok(()))
        .await
        .expect_err("not manual");
    assert_reason(&err, ErrorCode::Internal, "not_a_manual_run");
    t.drop_now().await;
}

/// THE secrets rule (ADR-0029, obligation of C3 / C6): only a push to a
/// branch that was protected at creation. Whoever hands a job its secrets
/// (the runner API, C3; the secret store, C6) asks
/// `StoredPipeline::secrets_allowed` and must keep this test green. The
/// table refuses a protected pipeline of another source, and a retry keeps
/// the answer of the creation.
#[tokio::test]
async fn only_a_protected_push_may_release_secrets() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let deploy = || Planned::Jobs(vec![a_job("deploy", &[])]);
    let stored = |id: i64| {
        let db = t.db.clone(); // clone: a handle for the read
        async move { get(&db, p, id).await.expect("get").expect("found").0 }
    };

    let mut push = new_pipeline(p, Source::Push, deploy());
    push.protected = true;
    let protected = create(&t.db, &push).await.expect("protected").pipeline_id;
    let mut plain = new_pipeline(p, Source::Push, deploy());
    plain.request.git_ref = branch("feature");
    let plain = create(&t.db, &plain).await.expect("plain").pipeline_id;
    let manual = create(&t.db, &new_pipeline(p, Source::Manual, deploy()))
        .await
        .expect("manual")
        .pipeline_id;
    assert!(stored(protected).await.secrets_allowed());
    assert!(
        !stored(plain).await.secrets_allowed(),
        "an unprotected push"
    );
    assert!(!stored(manual).await.secrets_allowed(), "a manual run");

    // Both columns decide: a row that says `protected` is still refused
    // when it is not a push (it cannot be stored either, below).
    for source in [Source::MergeRequest, Source::Manual, Source::Schedule] {
        let mut other = stored(protected).await;
        other.source = source;
        assert!(!other.secrets_allowed(), "{source:?}");
    }
    for source in [Source::Manual, Source::Schedule] {
        let mut wrong = new_pipeline(p, source, deploy());
        wrong.protected = true;
        wrong.request.git_ref = branch("release");
        let _ = create(&t.db, &wrong)
            .await
            .expect_err("the table refuses it");
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM forge.ci_pipelines")
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count");
    assert_eq!(rows, 3, "nothing was written for the refused ones");

    // Cancel and retry change neither column.
    let user = |r: &str| by(Actor::User(alice), r);
    for id in [protected, manual] {
        cancel_pipeline_decided(&t.db, &user("req-c"), (p, id), alice, |_| Ok(()))
            .await
            .expect("cancel");
        retry_pipeline_decided(&t.db, &user("req-r"), (p, id), alice, |_| Ok(()))
            .await
            .expect("retry");
    }
    assert!(stored(protected).await.secrets_allowed(), "after a retry");
    assert!(!stored(manual).await.secrets_allowed(), "after a retry");
    t.drop_now().await;
}

fn assert_reason_ctx(err: &forge_errors::ForgeError, key: &str, value: &str) {
    assert!(
        err.context().iter().any(|(k, v)| *k == key && v == value),
        "{key}={value} in {:?}",
        err.context()
    );
}

/// M4 C4 review: the maintenance sweep keeps only the refs of live
/// pipelines (`pending` / `running`); a finished or unknown id is not live.
#[tokio::test]
async fn only_pending_and_running_pipelines_are_live() {
    let t = FreshDb::migrated().await;
    let (_, web) = web(&t).await;
    let p = web.project_id;
    let mut ids = Vec::new();
    for (n, status) in ["pending", "running", "succeeded", "failed", "canceled"]
        .into_iter()
        .enumerate()
    {
        let mut new = new_pipeline(p, Source::Manual, Planned::Jobs(vec![a_job("build", &[])]));
        new.request.git_ref = branch(&format!("b{n}"));
        let id = create(&t.db, &new).await.expect("create").pipeline_id;
        let job = if status == "running" {
            "pending"
        } else {
            status
        };
        force_status(&t, id, status, job).await;
        ids.push(id);
    }
    let mut asked = ids.clone();
    asked.push(ids.iter().max().copied().unwrap_or(0) + 1000);
    let live = live_among(&t.db, &asked).await.expect("live");
    assert_eq!(live, ids[..2].to_vec());
    assert!(live_among(&t.db, &[]).await.expect("none").is_empty());
    t.drop_now().await;
}
