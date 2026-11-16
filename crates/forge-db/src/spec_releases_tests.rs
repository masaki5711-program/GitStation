//! Specification releases against a migrated database (M4 C12): the job
//! payload, and one `spec.released` audit row and event per release (a
//! retried job records nothing twice).

use forge_core::{ObjectId, RefName, RequestId, UserId};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

#[test]
fn the_publish_payload_round_trips_and_refuses_anything_else() {
    let request = PublishRequest {
        project: ProjectId::new(7),
        tag: RefName::parse("refs/tags/v1.5").expect("tag"),
        object: ObjectId::parse(&"d".repeat(40)).expect("sha"),
        released_by: Some(UserId::new(3)),
        push_request_id: Some("req-push-1".to_owned()),
    };
    let job = publish_job(&request, RequestId::generate(), Some(9)).expect("job");
    assert_eq!(job.kind.as_str(), SPEC_PUBLISH_JOB);
    assert_eq!(publish_payload(&job.payload).expect("parse"), request);
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"project_id": 7, "tag": "refs/heads/main", "object": "d".repeat(40)}),
        serde_json::json!({"project_id": 7, "tag": "refs/tags/v1", "object": "xyz"}),
        serde_json::json!({"project_id": 7, "tag": "refs/tags/v1", "object": "d".repeat(40),
            "released_by": 0}),
    ] {
        assert!(publish_payload(&bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn a_release_is_one_event_and_a_retry_records_nothing_more() {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &ctx("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let specs = projects::create(
        &t.db,
        &ctx("req-project-specs"),
        &NewProject::new("specs", alice.namespace_id, "Specs", alice.user_id),
    )
    .await
    .expect("specs");
    let release = NewRelease {
        project: specs.project_id,
        repository: specs.repository_id,
        tag: RefName::parse("refs/tags/v1.5").expect("tag"),
        version: "v1.5".to_owned(),
        commit: ObjectId::parse(&"e".repeat(40)).expect("sha"),
        released_by: Some(alice.user_id),
        default_branch: "refs/heads/main".to_owned(),
    };
    assert!(record_release(&t.db, &ctx("req-release"), &release)
        .await
        .expect("record"));
    assert!(!record_release(&t.db, &ctx("req-release-2"), &release)
        .await
        .expect("again"));
    // M4 C12 review: a version is released once — the same tag re-created
    // at another commit records nothing.
    let moved = NewRelease {
        commit: ObjectId::parse(&"f".repeat(40)).expect("sha"),
        ..release.clone()
    };
    assert!(!record_release(&t.db, &ctx("req-release-3"), &moved)
        .await
        .expect("moved tag"));
    // The pusher: a person is, an unknown account is not.
    assert!(pusher_is_person(&t.db, alice.user_id, "req-push-x")
        .await
        .expect("person"));
    assert!(!pusher_is_person(&t.db, UserId::new(999_999), "req-push-x")
        .await
        .expect("unknown"));
    let sa = crate::service_accounts::create(
        &t.db,
        &ctx("req-sa"),
        &crate::service_accounts::NewServiceAccount {
            username: "release-bot".to_owned(),
            display_name: "Release bot".to_owned(),
            owner: alice.user_id,
            purpose: "synthetic releases".to_owned(),
        },
        alice.user_id,
    )
    .await
    .expect("sa")
    .user_id;
    assert!(!pusher_is_person(&t.db, sa, "req-push-x")
        .await
        .expect("service account"));
    let events: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT event_type, payload FROM forge.events WHERE event_type = 'spec.released'",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1["version"], "v1.5");
    assert_eq!(events[0].1["sha"], "e".repeat(40));
    assert_eq!(events[0].1["released_by"], alice.user_id.get());
    let audit: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, actor_kind FROM forge.audit_log WHERE action = 'spec.released'",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("audit");
    assert_eq!(audit, [("spec.released".to_owned(), "system".to_owned())]);
    t.drop_now().await;
}
