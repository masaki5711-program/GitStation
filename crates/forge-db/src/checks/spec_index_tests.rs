//! The `spec-index` subscriber and the `check.spec_index` job (M3 X4):
//! every merge request version asks for one job of its head, a work item
//! link change asks for its current head again, and the payload is read
//! back exactly. Synthetic data only.

use forge_core::{
    IntegrationKind, LinkActorKind, ObjectId, ProjectId, RefName, RequestId, WorkItemRef,
};
use forge_errors::ErrorCode;

use super::spec_index::{
    spec_index_job, spec_index_payload, SpecIndexRequest, SpecIndexRequests, SPEC_INDEX_JOB,
    SPEC_INDEX_SUBSCRIBER,
};
use crate::change::{Actor, ChangeContext};
use crate::integrations::{self, NewIntegration};
use crate::projects::{self, NewProject};
use crate::review::{
    link_work_item_mr_decided, open_mr_decided, record_mr_version, NewMergeRequest,
};
use crate::service_accounts::{self, NewServiceAccount};
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

fn new_mr(source: &str) -> NewMergeRequest {
    NewMergeRequest {
        title: "Feature".to_owned(),
        description: String::new(),
        source_branch: RefName::parse(&format!("refs/heads/{source}")).expect("branch"),
        target_branch: RefName::parse("refs/heads/main").expect("branch"),
        draft: false,
    }
}

/// Number the events, then run the subscriber until it caught up.
async fn drain(t: &FreshDb, subscriber: &SpecIndexRequests) {
    loop {
        crate::sequencer::run_once(
            &t.db,
            crate::sequencer::BatchSize::MAX,
            crate::sequencer::AuditSequencing::Skip,
        )
        .await
        .expect("sequencer");
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

async fn jobs(t: &FreshDb) -> Vec<(String, SpecIndexRequest)> {
    let rows: Vec<(String, serde_json::Value)> =
        sqlx::query_as("SELECT dedup_key, payload FROM forge.jobs WHERE kind = $1 ORDER BY id")
            .bind(SPEC_INDEX_JOB)
            .fetch_all(&mut t.superuser().await)
            .await
            .expect("jobs");
    rows.into_iter()
        .map(|(key, payload)| (key, spec_index_payload(&payload).expect("payload")))
        .collect()
}

/// alice, her project `web` bound to kanflow project `kf-1`, and the
/// subscriber registered at the head.
async fn world() -> (FreshDb, forge_core::UserId, ProjectId) {
    let t = FreshDb::migrated().await;
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
    let sa = service_accounts::create(
        &t.db,
        &system("req-sa"),
        &NewServiceAccount {
            username: "kanflow-sync".to_owned(),
            display_name: "kanflow".to_owned(),
            owner: alice.user_id,
            purpose: "Kanflow reads the event feed".to_owned(),
        },
        alice.user_id,
    )
    .await
    .expect("service account")
    .user_id;
    let integration = integrations::create(
        &t.db,
        &system("req-int"),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        alice.user_id,
    )
    .await
    .expect("integration");
    let (_bound, _, ()) = integrations::bind_decided(
        &t.db,
        &system("req-bind"),
        web.project_id,
        alice.user_id,
        integration,
        "kf-1",
        |_| Ok(()),
    )
    .await
    .expect("bind");
    subscribers::register(&t.db, &SpecIndexRequests, Start::Head)
        .await
        .expect("register");
    (t, alice.user_id, web.project_id)
}

async fn link(
    t: &FreshDb,
    alice: forge_core::UserId,
    mr: (ProjectId, i64),
    request: &str,
    raw: &str,
) {
    let ctx = ChangeContext::new(Actor::User(alice), RequestId::parse(request).expect("id"));
    let _linked = link_work_item_mr_decided(
        &t.db,
        &ctx,
        mr,
        &WorkItemRef::parse(raw).expect("ref"),
        (alice, LinkActorKind::Human),
        |_| Ok(()),
    )
    .await
    .expect("link");
}

#[tokio::test]
async fn every_version_and_every_link_change_asks_for_one_spec_index_job() {
    let (t, alice, p) = world().await;
    assert_eq!(SpecIndexRequests.name(), SPEC_INDEX_SUBSCRIBER);
    let user = |request: &str| {
        ChangeContext::new(Actor::User(alice), RequestId::parse(request).expect("id"))
    };
    let mr = open_mr_decided(
        &t.db,
        &user("req-open"),
        p,
        alice,
        &new_mr("feature"),
        |_| Ok(()),
    )
    .await
    .map(|(stored, ())| stored)
    .expect("open");
    // A link before any version: passed over (the first version is
    // evaluated with the links of that time).
    link(&t, alice, (p, mr.iid), "req-link-early", "PBI-1").await;
    drain(&t, &SpecIndexRequests).await;
    assert!(jobs(&t).await.is_empty());
    for (request, head) in [("req-v1", 'a'), ("req-v2", 'b'), ("req-v3", 'a')] {
        record_mr_version(&t.db, &system(request), mr.id, &sha(head), Some(alice))
            .await
            .expect("version");
    }
    drain(&t, &SpecIndexRequests).await;
    let found = jobs(&t).await;
    let heads: Vec<&str> = found.iter().map(|(_, r)| r.head.as_str()).collect();
    // v3 went back to head `a` (no new version, no event): two jobs.
    assert_eq!(heads, [sha('a').as_str(), sha('b').as_str()]);
    assert_eq!(
        found[0].0,
        format!("spec_index:{}:{}:{}", p.get(), mr.iid, sha('a').as_str())
    );
    assert_eq!(found[0].1.iid, mr.iid);
    // A link now: the current head again, once per event.
    link(&t, alice, (p, mr.iid), "req-link-late", "PBI-2").await;
    drain(&t, &SpecIndexRequests).await;
    let found = jobs(&t).await;
    assert_eq!(found.len(), 3, "{found:?}");
    let (key, again) = &found[2];
    // v3 moved the head back to `a`: that is the current head.
    assert_eq!(again.head, sha('a'), "the merge request's current head");
    assert!(
        key.starts_with(&format!(
            "spec_index:{}:{}:{}:seq:",
            p.get(),
            mr.iid,
            sha('a').as_str()
        )),
        "{key}"
    );
    t.drop_now().await;
}

#[test]
fn the_payload_round_trips_and_refuses_anything_else() {
    let request = SpecIndexRequest {
        project: ProjectId::new(7),
        iid: 3,
        head: sha('d'),
    };
    let job = spec_index_job(&request, None, RequestId::generate(), Some(3)).expect("job");
    assert_eq!(job.kind.as_str(), SPEC_INDEX_JOB);
    assert_eq!(spec_index_payload(&job.payload).expect("parse"), request);
    let again = spec_index_job(&request, Some(42), RequestId::generate(), Some(42)).expect("job");
    assert_eq!(
        again.dedup_key.expect("key").as_str(),
        format!("spec_index:7:3:{}:seq:42", sha('d').as_str())
    );
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"project_id": 7, "iid": 0, "head_sha": sha('d').as_str()}),
        serde_json::json!({"project_id": 0, "iid": 1, "head_sha": sha('d').as_str()}),
        serde_json::json!({"project_id": 7, "iid": 1, "head_sha": "HEAD"}),
        serde_json::json!({"project_id": 7, "head_sha": sha('d').as_str()}),
    ] {
        let err = spec_index_payload(&bad).expect_err("bad payload");
        assert_reason(&err, ErrorCode::Internal, "invalid_job_payload");
    }
}
