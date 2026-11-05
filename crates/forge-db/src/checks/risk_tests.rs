//! `forge/risk`'s job, subscriber, targets and sweep query (M3 X7,
//! ADR-0023 Amendment X7).

use forge_core::{ObjectId, ProjectId, RefName, RequestId, UserId};
use forge_errors::ErrorCode;

use super::{
    risk_job, risk_payload, risk_targets, unanswered_dead_jobs, RiskRequest, RiskRequests,
    MAX_TARGETS, RISK_JOB, RISK_SUBSCRIBER,
};
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::{
    close_mr_decided, open_mr_decided, record_mr_version, NewMergeRequest, StoredMergeRequest,
};
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

fn new_mr(source: &str, target: &str) -> NewMergeRequest {
    NewMergeRequest {
        title: format!("Merge {source}"),
        description: String::new(),
        source_branch: RefName::parse(&format!("refs/heads/{source}")).expect("branch"),
        target_branch: RefName::parse(&format!("refs/heads/{target}")).expect("branch"),
        draft: false,
    }
}

async fn open(
    t: &FreshDb,
    p: ProjectId,
    alice: UserId,
    source: &str,
    target: &str,
) -> StoredMergeRequest {
    open_mr_decided(
        &t.db,
        &ChangeContext::new(
            Actor::User(alice),
            RequestId::parse(&format!("req-open-{source}")).expect("id"),
        ),
        p,
        alice,
        &new_mr(source, target),
        |_| Ok(()),
    )
    .await
    .map(|(stored, ())| stored)
    .expect("open")
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

#[test]
fn the_payload_round_trips_and_refuses_anything_else() {
    let request = RiskRequest {
        project: ProjectId::new(7),
        head: sha('d'),
    };
    let job = risk_job(&request, RequestId::generate(), Some(3)).expect("job");
    assert_eq!(job.kind.as_str(), RISK_JOB);
    assert_eq!(RISK_JOB, "check.risk");
    assert_eq!(
        job.dedup_key.as_ref().map(crate::jobs::DedupKey::as_str),
        Some(format!("risk:7:{}", sha('d').as_str()).as_str())
    );
    assert_eq!(risk_payload(&job.payload).expect("parse"), request);
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"project_id": 0, "head_sha": sha('d').as_str()}),
        serde_json::json!({"project_id": 7, "head_sha": "HEAD"}),
        serde_json::json!({"project_id": "7", "head_sha": sha('d').as_str()}),
    ] {
        let err = risk_payload(&bad).expect_err("bad payload");
        assert_reason(&err, ErrorCode::Internal, "invalid_job_payload");
    }
}

/// Every version of a merge request asks for one `check.risk` job of its
/// head; the same head is asked once (a force push back to an earlier head
/// is no new version).
#[tokio::test]
async fn every_merge_request_version_enqueues_one_risk_job_of_its_head() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let subscriber = RiskRequests;
    assert_eq!(subscriber.name(), RISK_SUBSCRIBER);
    subscribers::register(&t.db, &subscriber, Start::Head)
        .await
        .expect("register");
    let mr = open(&t, p, alice, "feature", "main").await;
    for (request, head) in [("req-v1", 'a'), ("req-v2", 'b'), ("req-v3", 'a')] {
        record_mr_version(&t.db, &system(request), mr.id, &sha(head), Some(alice))
            .await
            .expect("version");
    }
    drain(&t, &subscriber).await;
    let jobs: Vec<(String, serde_json::Value)> =
        sqlx::query_as("SELECT dedup_key, payload FROM forge.jobs WHERE kind = $1 ORDER BY id")
            .bind(RISK_JOB)
            .fetch_all(&mut t.superuser().await)
            .await
            .expect("jobs");
    let heads: Vec<String> = jobs
        .iter()
        .map(|(_, payload)| {
            let request = risk_payload(payload).expect("payload");
            assert_eq!(request.project, p);
            request.head.as_str().to_owned()
        })
        .collect();
    assert_eq!(heads, [sha('a').as_str(), sha('b').as_str()]);
    assert_eq!(jobs[0].0, format!("risk:{}:{}", p.get(), sha('a').as_str()));
    t.drop_now().await;
}

/// The branches a head is assessed against: those of the open merge
/// requests whose version it is; a closed one no longer counts.
#[tokio::test]
async fn the_targets_of_a_head_are_those_of_its_open_merge_requests() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let head = sha('a');
    let none = risk_targets(&t.db, p, &head).await.expect("targets");
    assert!(none.targets.is_empty() && !none.more);
    let to_main = open(&t, p, alice, "one", "main").await;
    let to_dev = open(&t, p, alice, "two", "dev").await;
    for mr in [&to_main, &to_dev] {
        record_mr_version(&t.db, &system("req-v"), mr.id, &head, Some(alice))
            .await
            .expect("version");
    }
    let both = risk_targets(&t.db, p, &head).await.expect("targets");
    let names: Vec<&str> = both.targets.iter().map(RefName::as_str).collect();
    assert_eq!(names, ["refs/heads/dev", "refs/heads/main"]);
    assert!(!both.more);
    // Another project's head is not this one's.
    assert!(risk_targets(&t.db, ProjectId::new(p.get() + 1), &head)
        .await
        .expect("targets")
        .targets
        .is_empty());
    let _closed = close_mr_decided(
        &t.db,
        &ChangeContext::new(
            Actor::User(alice),
            RequestId::parse("req-close").expect("id"),
        ),
        p,
        to_dev.iid,
        alice,
        |_| Ok(()),
    )
    .await
    .expect("close");
    let one = risk_targets(&t.db, p, &head).await.expect("targets");
    let names: Vec<&str> = one.targets.iter().map(RefName::as_str).collect();
    assert_eq!(names, ["refs/heads/main"]);
    t.drop_now().await;
}

#[tokio::test]
async fn more_targets_than_the_cap_are_cut_and_say_so() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let head = sha('b');
    for i in 0..=MAX_TARGETS {
        let mr = open(&t, p, alice, &format!("f{i}"), &format!("t{i}")).await;
        record_mr_version(&t.db, &system("req-v"), mr.id, &head, Some(alice))
            .await
            .expect("version");
    }
    let found = risk_targets(&t.db, p, &head).await.expect("targets");
    assert_eq!(found.targets.len(), MAX_TARGETS);
    assert!(found.more);
    t.drop_now().await;
}

/// The sweep's query: a dead job whose head has no `forge/risk` run.
#[tokio::test]
async fn a_dead_risk_job_without_a_run_is_unanswered() {
    let t = FreshDb::migrated().await;
    let (_, web) = web(&t).await;
    let p = web.project_id;
    let request = RiskRequest {
        project: p,
        head: sha('c'),
    };
    let job = risk_job(
        &request,
        RequestId::parse("req-risk-dead").expect("id"),
        None,
    )
    .expect("job");
    crate::jobs::enqueue(&t.db, &job).await.expect("enqueue");
    assert!(unanswered_dead_jobs(&t.db, 10)
        .await
        .expect("none")
        .is_empty());
    t.exec_superuser(
        "UPDATE forge.jobs SET state = 'dead', finished_at = now(), \
         last_error_code = 'FG-DB-0007' WHERE kind = 'check.risk'",
    )
    .await;
    let found = unanswered_dead_jobs(&t.db, 10).await.expect("unanswered");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].request, request);
    assert_eq!(found[0].origin_request_id, "req-risk-dead");
    assert_eq!(found[0].last_error_code, "FG-DB-0007");
    // An answer on the head (any conclusion) ends it.
    crate::checks::record_forge_check_run(
        &t.db,
        &system("req-answer"),
        (p, &request.head),
        &crate::checks::NewCheckRun {
            name: forge_core::CheckName::parse(forge_core::checks::RISK_CHECK).expect("name"),
            status: forge_core::checks::CheckStatus::Completed,
            conclusion: Some(forge_core::checks::CheckConclusion::Error),
            summary: "synthetic".to_owned(),
            details_url: None,
            annotations: Vec::new(),
        },
    )
    .await
    .expect("answer");
    assert!(unanswered_dead_jobs(&t.db, 10)
        .await
        .expect("answered")
        .is_empty());
    t.drop_now().await;
}
