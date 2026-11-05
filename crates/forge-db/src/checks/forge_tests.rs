//! forge's own check runs (M3 X2, migration 20261124100000): written by
//! the worker without a reporter, and the `spec-drift` subscriber's job of
//! each merge request version.

use forge_core::checks::{CheckAnnotationLevel, CheckConclusion, CheckStatus};
use forge_core::{CheckName, ObjectId, ProjectId, RefName, RequestId, UserId};
use forge_errors::ErrorCode;

use super::spec_drift::{
    spec_drift_job, spec_drift_payload, SpecDriftRequest, SpecDriftRequests, SPEC_DRIFT_JOB,
    SPEC_DRIFT_SUBSCRIBER,
};
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

fn drift(conclusion: CheckConclusion, annotations: Vec<CheckAnnotation>) -> NewCheckRun {
    NewCheckRun {
        name: CheckName::parse(forge_core::checks::SPEC_DRIFT_CHECK).expect("name"),
        status: CheckStatus::Completed,
        conclusion: Some(conclusion),
        summary: "synthetic drift summary".to_owned(),
        details_url: None,
        annotations,
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

async fn scalar(t: &FreshDb, sql: &str, project: ProjectId) -> i64 {
    sqlx::query_scalar(sql)
        .bind(project.get())
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count")
}

#[tokio::test]
async fn a_forge_check_run_is_recorded_without_a_reporter() {
    let t = FreshDb::migrated().await;
    let (_, web) = web(&t).await;
    let p = web.project_id;
    let head = sha('a');
    let note = CheckAnnotation {
        path: ".forge/repo.yml".to_owned(),
        start_line: 2,
        end_line: 2,
        level: CheckAnnotationLevel::Warning,
        message: "synthetic".to_owned(),
        title: None,
    };
    let first = record_forge_check_run(
        &t.db,
        &system("req-drift-1"),
        (p, &head),
        &drift(CheckConclusion::Neutral, vec![note]),
    )
    .await
    .expect("first");
    assert!(first.created);
    assert!(
        first.completed_event,
        "no conclusion before: check.completed"
    );
    assert_eq!(first.run.reported_by, None);
    assert_eq!(first.run.reported_by_username, None);
    assert_eq!(first.run.annotations.len(), 1);
    // The same conclusion again: the row is updated, no second event.
    let again = record_forge_check_run(
        &t.db,
        &system("req-drift-2"),
        (p, &head),
        &drift(CheckConclusion::Neutral, Vec::new()),
    )
    .await
    .expect("again");
    assert!(!again.created && !again.completed_event);
    assert_eq!(again.run.revision, 2);
    assert!(again.run.annotations.is_empty());
    // The runs read back like any other (the merge status, `GET …/checks`).
    let listed = check_runs(&t.db, p, &head).await.expect("list");
    assert_eq!(listed, vec![again.run.clone()]);
    assert_eq!(
        scalar(
            &t,
            "SELECT count(*) FROM forge.events WHERE project_id = $1 \
             AND event_type = 'check.completed' AND actor_kind = 'system'",
            p
        )
        .await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn only_the_names_forge_reports_are_recorded_by_forge() {
    let t = FreshDb::migrated().await;
    let (_, web) = web(&t).await;
    for raw in ["ci/gates", "forge/risk"] {
        let mut new = drift(CheckConclusion::Success, Vec::new());
        new.name = CheckName::parse(raw).expect("name");
        let err =
            record_forge_check_run(&t.db, &system("req-x"), (web.project_id, &sha('b')), &new)
                .await
                .expect_err(raw);
        assert_reason(&err, ErrorCode::Internal, "not_forge_check");
    }
    let absent = record_forge_check_run(
        &t.db,
        &system("req-gone"),
        (ProjectId::new(web.project_id.get() + 1000), &sha('b')),
        &drift(CheckConclusion::Success, Vec::new()),
    )
    .await
    .expect_err("absent project");
    assert_eq!(absent.code(), ErrorCode::NotFound);
    assert_eq!(
        scalar(
            &t,
            "SELECT count(*) FROM forge.check_runs WHERE project_id = $1",
            web.project_id
        )
        .await,
        0
    );
    t.drop_now().await;
}

/// The backstop: a `forge/` run has no reporter, every other one has one.
#[tokio::test]
async fn the_table_keeps_reporters_for_every_namespace_but_forge() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id.get();
    let a = alice.get();
    let mut su = t.superuser().await;
    for (name, reporter, credential) in [
        ("ci/gates", "NULL", "NULL"),
        ("forge/spec-drift", &*a.to_string(), "NULL"),
        ("forge/spec-drift", "NULL", "7"),
    ] {
        let sql = format!(
            "INSERT INTO forge.check_runs (project_id, head_sha, name, status, reported_by, \
             credential_id, request_id) VALUES ({p}, repeat('c', 40), '{name}', 'queued', \
             {reporter}, {credential}, 'req-x')"
        );
        let constraint = match sqlx::query(&sql).execute(&mut su).await {
            Err(sqlx::Error::Database(e)) => e.constraint().map(str::to_owned),
            _ => None,
        };
        assert_eq!(constraint.as_deref(), Some("check_runs_reporter"), "{sql}");
    }
    drop(su);
    t.drop_now().await;
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

/// Every version of a merge request (its `merge_request.pushed`) asks for
/// one `check.spec_drift` job of its head; the same head is asked once.
#[tokio::test]
async fn every_merge_request_version_enqueues_one_drift_job_of_its_head() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let subscriber = SpecDriftRequests;
    assert_eq!(subscriber.name(), SPEC_DRIFT_SUBSCRIBER);
    subscribers::register(&t.db, &subscriber, Start::Head)
        .await
        .expect("register");
    let mr = open_mr_decided(
        &t.db,
        &ChangeContext::new(
            Actor::User(alice),
            RequestId::parse("req-open").expect("id"),
        ),
        p,
        alice,
        &new_mr("feature"),
        |_| Ok(()),
    )
    .await
    .map(|(stored, ())| stored)
    .expect("open");
    for (request, head) in [("req-v1", 'a'), ("req-v2", 'b'), ("req-v3", 'a')] {
        record_mr_version(&t.db, &system(request), mr.id, &sha(head), Some(alice))
            .await
            .expect("version");
    }
    drain(&t, &subscriber).await;
    let jobs: Vec<(String, serde_json::Value)> =
        sqlx::query_as("SELECT dedup_key, payload FROM forge.jobs WHERE kind = $1 ORDER BY id")
            .bind(SPEC_DRIFT_JOB)
            .fetch_all(&mut t.superuser().await)
            .await
            .expect("jobs");
    let heads: Vec<String> = jobs
        .iter()
        .map(|(_, payload)| {
            let request = spec_drift_payload(payload).expect("payload");
            assert_eq!(request.project, p);
            request.head.as_str().to_owned()
        })
        .collect();
    // v3 went back to head `a` (no new version, no event): two jobs.
    assert_eq!(heads, [sha('a').as_str(), sha('b').as_str()]);
    assert_eq!(
        jobs[0].0,
        format!("spec_drift:{}:{}", p.get(), sha('a').as_str())
    );
    t.drop_now().await;
}

#[test]
fn the_payload_round_trips_and_refuses_anything_else() {
    let request = SpecDriftRequest {
        project: ProjectId::new(7),
        head: sha('d'),
    };
    let job = spec_drift_job(&request, RequestId::generate(), Some(3)).expect("job");
    assert_eq!(job.kind.as_str(), SPEC_DRIFT_JOB);
    assert_eq!(spec_drift_payload(&job.payload).expect("parse"), request);
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"project_id": 0, "head_sha": sha('d').as_str()}),
        serde_json::json!({"project_id": 7, "head_sha": "HEAD"}),
        serde_json::json!({"project_id": "7", "head_sha": sha('d').as_str()}),
    ] {
        let err = spec_drift_payload(&bad).expect_err("bad payload");
        assert_reason(&err, ErrorCode::Internal, "invalid_job_payload");
    }
}

/// Review X2 (security HIGH): other reporters cannot crowd forge's own run
/// out. 100 `ci/` runs fill the reporters' cap; forge's run is still
/// recorded, every read sees all 101, and a 101st reporter name is still
/// refused.
#[tokio::test]
async fn forge_runs_never_count_against_the_reporters_cap() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let head = sha('f');
    t.commit_audited(
        ("project", p.get()),
        &[format!(
            "INSERT INTO forge.check_runs (project_id, head_sha, name, status, reported_by,              request_id) SELECT {}, '{}', 'ci/n' || g, 'queued', {}, 'req-fill'              FROM generate_series(1, 100) g",
            p.get(),
            head.as_str(),
            alice.get()
        )],
    )
    .await;
    let recorded = record_forge_check_run(
        &t.db,
        &system("req-drift-full"),
        (p, &head),
        &drift(CheckConclusion::Error, Vec::new()),
    )
    .await
    .expect("forge's run is recorded on a full commit");
    assert!(recorded.created);
    assert_eq!(check_runs(&t.db, p, &head).await.expect("list").len(), 101);
    let snapshot = runs_for(&t.db, p, &head).await.expect("merge status");
    assert_eq!(snapshot.len(), 101);
    let err = report_check_run_decided(
        &t.db,
        &ChangeContext::new(
            Actor::User(alice),
            RequestId::parse("req-over").expect("id"),
        ),
        (p, &head),
        alice,
        ReportCredential(None),
        &NewCheckRun {
            name: CheckName::parse("ci/one-more").expect("name"),
            ..drift(CheckConclusion::Success, Vec::new())
        },
        |_| Ok(()),
    )
    .await
    .expect_err("the reporters' cap still holds");
    assert_reason(&err, ErrorCode::CheckRunLimitReached, "too_many_runs");
    t.drop_now().await;
}
