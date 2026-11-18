//! The wake hub (plan 18 S2): its first connect and every reconnect wake
//! every waiter once, a lost link is logged with a registered code and
//! leaves waiters to their own poll, a burst is one wake, payloads are
//! bounded, and the doctor check alerts at a half-full NOTIFY queue.

use serde_json::Value;

use super::*;
use crate::jobs::runner_stop_tests::Captured;
use crate::test_support::*;

/// Far longer than a connect (or a reconnect) on the test server takes.
const SOON: Duration = Duration::from_secs(30);
const QUIET: Duration = Duration::from_millis(300);

fn context(event: &Value) -> String {
    event["fields"]["context"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The failures the hub logged about its listener.
fn listener_failures(logs: &Captured) -> Vec<Value> {
    logs.failures()
        .into_iter()
        .filter(|e| context(e).contains("operation=wake_listen"))
        .collect()
}

#[tokio::test]
async fn a_reconnected_listener_looks_at_once() {
    let logs = Captured::default();
    let _guard = logs.install();
    let t = FreshDb::migrated().await;
    let hub = WakeHub::spawn(&t.db, &Channel::ALL).expect("hub");
    // Subscribed before the hub is connected: both are told to look.
    let mut jobs = hub.subscribe(Channel::Jobs);
    let mut events = hub.subscribe_unseen(Channel::Events);
    assert_eq!(jobs.wait(SOON).await, Woken::Reconnected);
    assert_eq!(events.wait(SOON).await, Woken::Reconnected);
    assert!(hub.is_live(Channel::Jobs) && hub.is_live(Channel::Events));
    let first = hub.backend_pid().expect("connected");
    // Connected and quiet: nothing wakes a waiter, nothing is logged.
    assert_eq!(jobs.wait(QUIET).await, Woken::SafetyNet);
    assert!(listener_failures(&logs).is_empty(), "{:?}", logs.failures());

    // The server ends the LISTEN connection.
    t.exec_superuser(&format!("SELECT pg_catalog.pg_terminate_backend({first})"))
        .await;
    let started = std::time::Instant::now();
    // The loss ends the wait once (the waiter polls by itself meanwhile),
    // then the reconnect tells it to look.
    let mut woken = Vec::new();
    while woken.last() != Some(&Woken::Reconnected) {
        assert!(woken.len() < 3, "{woken:?}");
        woken.push(jobs.wait(SOON).await);
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "reconnected after {:?}: {woken:?}",
        started.elapsed()
    );
    // Every waiter, of every channel.
    let mut others = Vec::new();
    while others.last() != Some(&Woken::Reconnected) {
        assert!(others.len() < 3, "{others:?}");
        others.push(events.wait(SOON).await);
    }
    assert!(hub.is_live(Channel::Jobs));
    let second = hub.backend_pid().expect("connected again");
    assert_ne!(first, second);

    // The dead listener was visible: a registered code, once.
    let failures = listener_failures(&logs);
    assert_eq!(failures.len(), 1, "{failures:?}");
    let code = failures[0]["fields"]["error_code"]
        .as_str()
        .unwrap_or_default();
    assert!(matches!(code, "FG-DB-0001" | "FG-DB-0002"), "{failures:?}");
    let listening: Vec<Value> = logs
        .events()
        .into_iter()
        .filter(|e| e["fields"]["event"] == "forge.wake.listening")
        .collect();
    assert_eq!(listening.len(), 2, "{listening:?}");
    assert_eq!(listening[1]["fields"]["recovered"], true, "{listening:?}");

    // And notifications arrive again.
    t.exec_superuser("SELECT pg_catalog.pg_notify('forge_jobs', 'test.a')")
        .await;
    assert_eq!(
        jobs.wait(SOON).await,
        Woken::Notified(vec!["test.a".to_owned()])
    );
    hub.stop(SOON).await.expect("stop");
    t.drop_now().await;
}

#[tokio::test]
async fn doctor_alerts_when_the_notify_queue_is_half_full() {
    check_notify_queue(0.0).expect("empty");
    check_notify_queue(0.499).expect("under half");
    for (usage, shown) in [(0.5, "0.500"), (0.75, "0.750"), (1.0, "1.000")] {
        let e = check_notify_queue(usage).expect_err("half full");
        assert_eq!(e.code(), ErrorCode::NotifyQueueBacklog, "{usage}");
        assert_eq!(e.code().id(), "FG-DB-0023");
        let has = |key: &str, value: &str| e.context().iter().any(|(k, v)| *k == key && v == value);
        assert!(has("usage", shown) && has("threshold", "0.5"), "{e:?}");
    }
    // What the check is given: the server's own figure.
    let t = FreshDb::migrated().await;
    let usage = notify_queue_usage(&t.db).await.expect("usage");
    assert!((0.0..=1.0).contains(&usage), "{usage}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_burst_of_notifications_is_one_wake() {
    let t = FreshDb::migrated().await;
    let hub = WakeHub::spawn(&t.db, &[Channel::Jobs]).expect("hub");
    let mut rx = hub.subscribe_unseen(Channel::Jobs);
    assert_eq!(rx.wait(SOON).await, Woken::Reconnected);
    // 1,000 notifications while the waiter is busy elsewhere.
    for i in 0..1000 {
        hub.nudge(Channel::Jobs, if i % 2 == 0 { "test.a" } else { "test.b" });
    }
    hub.nudge(Channel::Jobs, "test.a");
    // One wake with the distinct payloads, in the order of their last
    // notification; then nothing is left.
    assert_eq!(
        rx.wait(SOON).await,
        Woken::Notified(vec!["test.b".to_owned(), "test.a".to_owned()])
    );
    assert_eq!(rx.wait(QUIET).await, Woken::SafetyNet);
    // A waiter that wants other payloads sleeps through them.
    hub.nudge(Channel::Jobs, "test.b");
    assert_eq!(
        rx.wait_matching(QUIET, |p| p == "test.a").await,
        Woken::SafetyNet
    );
    // A channel the hub does not listen on is never live.
    assert!(hub.listens(Channel::Jobs) && !hub.listens(Channel::Refs));
    assert!(!hub.is_live(Channel::Refs));
    hub.stop(SOON).await.expect("stop");
    t.drop_now().await;
}

#[tokio::test]
async fn a_stopped_hub_leaves_waiters_to_their_own_poll() {
    let t = FreshDb::migrated().await;
    let hub = WakeHub::spawn(&t.db, &Channel::ALL).expect("hub");
    let mut rx = hub.subscribe_unseen(Channel::Refs);
    assert_eq!(rx.wait(SOON).await, Woken::Reconnected);
    hub.stop(SOON).await.expect("stop");
    hub.stop(SOON)
        .await
        .expect("a second stop has nothing to do");
    assert!(!hub.is_live(Channel::Refs));
    assert_eq!(hub.backend_pid(), None);
    // The loss ends one wait at once; after that a wait runs its whole
    // period (never a spin).
    assert_eq!(rx.wait(SOON).await, Woken::SafetyNet);
    let started = std::time::Instant::now();
    assert_eq!(rx.wait(QUIET).await, Woken::SafetyNet);
    assert!(started.elapsed() >= QUIET);
    // No channel: refused before anything is started.
    let e = WakeHub::spawn(&t.db, &[]).expect_err("no channel");
    assert_eq!(e.code(), ErrorCode::Internal);
    t.drop_now().await;
}

#[tokio::test]
async fn a_payload_is_a_short_kind_or_id() {
    let t = FreshDb::migrated().await;
    let hub = WakeHub::spawn(&t.db, &[Channel::Refs]).expect("hub");
    let mut rx = hub.subscribe_unseen(Channel::Refs);
    assert_eq!(rx.wait(SOON).await, Woken::Reconnected);
    let mut conn = t.db.write_pool().acquire().await.expect("connection");
    let e = notify_on(&mut conn, Channel::Refs, &"x".repeat(PAYLOAD_MAX + 1))
        .await
        .expect_err("too long");
    assert_eq!(e.code(), ErrorCode::Internal);
    assert!(
        e.context()
            .iter()
            .any(|(k, v)| *k == "reason" && v == "payload_too_long"),
        "{e:?}"
    );
    let longest = "x".repeat(PAYLOAD_MAX);
    notify_on(&mut conn, Channel::Refs, &longest)
        .await
        .expect("the longest payload");
    assert_eq!(rx.wait(SOON).await, Woken::Notified(vec![longest]));
    hub.stop(SOON).await.expect("stop");
    t.drop_now().await;
}

#[test]
fn channels_are_plain_identifiers() {
    for channel in Channel::ALL {
        let name = channel.name();
        assert!(name.len() < 64, "{name}");
        assert!(
            name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{name}"
        );
        assert_eq!(Channel::of(name), Some(channel));
    }
    assert_eq!(Channel::of("forge_other"), None);
    assert_eq!(Channel::Jobs.name(), "forge_jobs");
}

#[test]
fn payloads_keep_the_last_of_each_and_forget_the_oldest() {
    let mut p = Payloads::default();
    assert_eq!(p.since(0), (0, Some(Vec::new())));
    for payload in ["a", "b", "a"] {
        p.push(payload);
    }
    assert_eq!(p.since(0), (3, Some(vec!["b".to_owned(), "a".to_owned()])));
    assert_eq!(p.since(2), (3, Some(vec!["a".to_owned()])));
    assert_eq!(p.since(3), (3, Some(Vec::new())));
    // More distinct payloads than are remembered: a waiter that far behind
    // is told to look at everything, one that kept up is not.
    for i in 0..=PAYLOADS_MAX {
        p.push(&format!("k{i}"));
    }
    assert_eq!(p.last.len(), PAYLOADS_MAX);
    let (reached, newer) = p.since(0);
    assert_eq!((reached, newer), (p.seq, None));
    let (_, newer) = p.since(p.seq - 1);
    assert_eq!(newer, Some(vec![format!("k{PAYLOADS_MAX}")]));
}

#[test]
fn a_missed_wakeup_is_logged_at_most_once_a_minute() {
    let at = Instant::now();
    assert!(may_log_missed(None, at));
    assert!(!may_log_missed(Some(at), at));
    assert!(!may_log_missed(Some(at), at + Duration::from_secs(59)));
    assert!(may_log_missed(Some(at), at + MISSED_LOG_EVERY));
}
