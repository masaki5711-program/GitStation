//! The worker's notification lane (M3 N1) against a migrated database and an
//! in-process relay: with `notify.enabled` a merge request event becomes a
//! mail through the real worker (sequencer, subscriber, the lane's own job
//! runner, the SMTP sender), and without the lane nothing records or sends.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

use std::time::{Duration, Instant};

use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::subscribers::states;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_notify::test_support::{FakeSmtp, Script};
use tokio::sync::watch;

use super::notify_lane::NotifyLane;
use super::seal_tests::seal_keys;
use super::{registry, registry_with, run_with_notify};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

#[tokio::test]
async fn the_worker_turns_an_approval_event_into_a_mail() {
    let t = FreshDb::migrated().await;
    let human = |name: &str| NewUser::human(name, name, &format!("{name}@example.test"));
    let alice = users::create(&t.db, &system("req-alice"), &human("alice"))
        .await
        .expect("alice");
    let bob = users::create(&t.db, &system("req-bob"), &human("bob"))
        .await
        .expect("bob");
    let web = projects::create(
        &t.db,
        &system("req-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    // A merge request as the command leaves it (counter + row, audited).
    let p = web.project_id.get();
    t.commit_audited(
        ("project", p),
        &[
            format!(
                "INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 1)"
            ),
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch) VALUES ({p}, 1, 'Fix rounding', {}, \
                 'refs/heads/fix-rounding', 'refs/heads/main')",
                alice.user_id.get()
            ),
        ],
    )
    .await;
    let iid = 1_i64;

    let relay = FakeSmtp::start(Script::default()).await;
    let lane = NotifyLane {
        cfg: relay.config(),
        origin: Some("https://git.example.test".to_owned()),
    };
    let (stop, stopped) = watch::channel(false);
    let db = t.db.clone(); // clone: the worker owns its pool handles
    let worker =
        tokio::spawn(async move { run_with_notify(&db, &lane, None, seal_keys(), stopped).await });

    // The subscribers register at the head when the worker starts: only
    // events after that are told.
    let started = Instant::now();
    loop {
        let known = states(&t.db).await.expect("states").unwrap_or_default();
        if known.iter().any(|s| s.name == "notify") {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no notify subscriber"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    t.exec_superuser(&format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('system', 'merge_request.probe', 'succeeded', 'merge_request', 'req-n1-w'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, project_id, actor_kind, actor_user_id, request_id, payload) VALUES \
         (gen_random_uuid(), 'merge_request.approved', 1, 'merge_request', 1, {}, 'user', {}, \
         'req-n1-w', '{{\"merge_request_id\": 1, \"iid\": {iid}, \"approver_id\": {}}}'::jsonb)",
        web.project_id.get(),
        bob.user_id.get(),
        bob.user_id.get(),
    ))
    .await;

    let started = Instant::now();
    while relay.messages().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "no mail arrived; notifications: {:?}",
            t.json_rows("SELECT state, reason FROM forge.notifications")
                .await
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let got = relay.messages();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].rcpt_to, ["alice@example.test"]);
    assert!(got[0]
        .data
        .contains("Message-ID: <notification-1@git.example.test>"));
    // The state is written just after the hand-over; give it a moment.
    let started = Instant::now();
    let mut state = String::new();
    while state != "sent" && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let rows = t.json_rows("SELECT state FROM forge.notifications").await;
        state = rows[0]["state"].as_str().unwrap_or_default().to_owned();
    }
    assert_eq!(state, "sent");

    stop.send(true).expect("stop");
    tokio::time::timeout(Duration::from_secs(60), worker)
        .await
        .expect("the worker stops")
        .expect("task")
        .expect("clean stop");
    assert_eq!(relay.messages().len(), 1, "one mail, once");
    t.drop_now().await;
}

#[test]
fn without_the_lane_no_notify_subscriber_is_registered() {
    let review = forge_core::config::ReviewConfig::default();
    let names = |r: &forge_db::subscribers::Registry| -> Vec<&'static str> {
        r.iter().map(|s| s.name()).collect()
    };
    let base = registry(&review).expect("registry");
    assert!(!names(&base).contains(&"notify"), "{:?}", names(&base));
    assert_eq!(
        names(&registry_with(&review, None, None).expect("registry")),
        names(&base)
    );
}

#[tokio::test]
async fn a_disabled_lane_still_registers_the_paused_subscriber() {
    let t = FreshDb::migrated().await;
    let review = forge_core::config::ReviewConfig::default();
    let registry = registry_with(&review, None, Some(&t.db)).expect("registry");
    let names: Vec<&'static str> = registry.iter().map(|s| s.name()).collect();
    assert!(names.contains(&"notify"), "{names:?}");
    t.drop_now().await;
}
