//! The wake hub (plan 18 S2): its first connect and every reconnect wake
//! every waiter once, a lost link is logged with a registered code and
//! leaves waiters to their own poll, a burst is one wake, payloads are
//! bounded, and the doctor check alerts at a half-full NOTIFY queue.
//! Review fixes: a listening task that ends is logged once and started
//! again while waiters poll, a flapping link is one lasting failure, the
//! hub's own sample logs a filling NOTIFY queue, and a logged miss carries
//! how many were not logged.

use std::sync::atomic::AtomicUsize;

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

/// The failures the hub logged about its task.
fn task_failures(logs: &Captured) -> Vec<Value> {
    logs.failures()
        .into_iter()
        .filter(|e| context(e).contains("task=wake_hub"))
        .collect()
}

/// Wait until `ready`, at most [`SOON`].
async fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !ready() {
        assert!(started.elapsed() < SOON, "{what}: not within {SOON:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Review fix 1: a listening task that ends by itself — here a panic — is
/// logged at once and once, with a registered code; the waiters fall back
/// to their own poll and keep working; the task is started again and
/// tells them to look.
#[tokio::test]
async fn a_hub_task_that_ends_is_logged_once_and_waiters_poll() {
    const POLL: Duration = Duration::from_millis(100);
    let logs = Captured::default();
    let _guard = logs.install();
    let t = FreshDb::migrated().await;
    let fail = Arc::new(tokio::sync::Notify::new());
    let runs = Arc::new(AtomicUsize::new(0));
    let body = {
        // clone: the task body shares the test's switch and counter
        let (fail, runs) = (Arc::clone(&fail), Arc::clone(&runs));
        move |shared: Arc<Shared>, link: Arc<Link>, stop: watch::Receiver<bool>| {
            let first = runs.fetch_add(1, Ordering::SeqCst) == 0;
            let fail = Arc::clone(&fail); // clone: one per run of the task
            async move {
                if !first {
                    return run(shared, link, stop).await;
                }
                tokio::select! {
                    () = run(shared, link, stop) => {}
                    () = fail.notified() => panic!("the listening task ends (test)"),
                }
            }
        }
    };
    let hub = WakeHub::spawn_running(&t.db, &[Channel::Jobs], body).expect("hub");
    let mut rx = hub.subscribe_unseen(Channel::Jobs);
    assert_eq!(rx.wait(SOON).await, Woken::Reconnected);
    assert!(hub.is_live(Channel::Jobs));
    assert!(task_failures(&logs).is_empty(), "{:?}", logs.failures());

    fail.notify_one();
    // The end of the task ends the wait once, and the hub is down.
    assert_eq!(rx.wait(SOON).await, Woken::SafetyNet);
    assert!(!hub.is_live(Channel::Jobs));
    assert_eq!(hub.backend_pid(), None);
    // It is visible at once, not at shutdown.
    eventually("the end of the task is logged", || {
        !task_failures(&logs).is_empty()
    })
    .await;
    let failures = task_failures(&logs);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0]["fields"]["error_code"], "FG-SYS-0001");
    assert_eq!(failures[0]["level"], "ERROR", "{failures:?}");
    let said = context(&failures[0]);
    for part in ["reason=task_panicked", "operation=wake_listen"] {
        assert!(said.contains(part), "{part} in {said}");
    }
    // The waiters keep working on their own poll: a runner's long poll
    // looks every POLL, not after the safety period.
    let db = t.db.clone().with_wake(hub.clone(), SOON); // clone: handles
    let mut idle = crate::ci_runners::JobWait::new(&db, POLL);
    let started = std::time::Instant::now();
    idle.wait(SOON).await;
    let waited = started.elapsed();
    assert!(waited >= POLL && waited < RECONNECT_MAX, "{waited:?}");

    // The task is started again: the waiters are told to look.
    assert_eq!(rx.wait(SOON).await, Woken::Reconnected);
    assert!(hub.is_live(Channel::Jobs));
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    t.exec_superuser("SELECT pg_catalog.pg_notify('forge_jobs', 'test.a')")
        .await;
    assert_eq!(
        rx.wait(SOON).await,
        Woken::Notified(vec!["test.a".to_owned()])
    );
    hub.stop(SOON).await.expect("stop");
    // Logged once: neither the restart nor the stop says it again.
    assert_eq!(task_failures(&logs).len(), 1, "{:?}", logs.failures());
    t.drop_now().await;
}

/// Review fix 1: a listening task that returns without being asked to is
/// logged too (no database needed: the task never connects).
#[tokio::test]
async fn a_hub_task_that_returns_unasked_is_logged() {
    let logs = Captured::default();
    let _guard = logs.install();
    let shared = Arc::new(Shared::new(&[Channel::Jobs]));
    let link = Arc::new(Link {
        options: PgConnectOptions::new(),
        round_trip: QUIET,
        target: "db.test".to_owned(),
        channels: vec![Channel::Jobs],
        request_id: RequestId::generate(),
    });
    let (stop, stopping) = watch::channel(false);
    let runs = Arc::new(AtomicUsize::new(0));
    let body = {
        let runs = Arc::clone(&runs); // clone: the counter is shared
        move |_: Arc<Shared>, _: Arc<Link>, _: watch::Receiver<bool>| {
            runs.fetch_add(1, Ordering::SeqCst);
            async {}
        }
    };
    let supervisor = tokio::spawn(supervise(Arc::clone(&shared), link, stopping, body));
    eventually("the end of the task is logged", || {
        !task_failures(&logs).is_empty()
    })
    .await;
    let failures = task_failures(&logs);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0]["fields"]["error_code"], "FG-SYS-0001");
    assert!(
        context(&failures[0]).contains("reason=task_ended"),
        "{failures:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    // A requested stop ends the supervisor during its pause, silently.
    stop.send_replace(true);
    tokio::time::timeout(SOON, supervisor)
        .await
        .expect("the supervisor ends")
        .expect("without a panic");
    assert_eq!(task_failures(&logs).len(), 1, "{:?}", logs.failures());
}

/// Review fix 4: a link that connects and drops again every few seconds
/// is ONE lasting failure — logged once, "recovered" said once, again
/// only every [`RELOG_EVERY`] — and a link that held is a new one.
#[test]
fn a_flapping_link_is_one_lasting_failure() {
    let logs = Captured::default();
    let _guard = logs.install();
    let rid = RequestId::generate();
    let lost = || unavailable("db.test", "listen_connection_lost");
    let start = Instant::now();
    let mut relog = Relog::default();
    assert_eq!(relog.up(), Some(false), "the first connect");
    // Up and down every five seconds for most of a minute.
    let mut logged = 0;
    let mut lines = Vec::new();
    for cycle in 0..12 {
        if relog.report_at(lost(), &rid, start + RECONNECT_MAX * cycle) {
            logged += 1;
        }
        lines.push(relog.up());
    }
    assert_eq!(logged, 1);
    assert_eq!(lines[0], Some(true), "{lines:?}");
    assert!(lines[1..].iter().all(Option::is_none), "{lines:?}");
    // Still flapping a minute later: said again, once.
    assert!(relog.report_at(lost(), &rid, start + RELOG_EVERY));
    assert_eq!(relog.up(), Some(true));
    let later = start + RELOG_EVERY + RECONNECT_MAX;
    assert!(!relog.report_at(lost(), &rid, later));
    assert_eq!(relog.up(), None);
    // A link that held for RECONNECT_MAX: its loss is new, logged at once.
    relog.settled();
    assert!(relog.report_at(lost(), &rid, later));
    assert_eq!(relog.up(), Some(true));
    // And another reason is never hidden by the limiter.
    assert!(relog.report_at(unavailable("db.test", "listen_ping_timeout"), &rid, later));
    let failures = listener_failures(&logs);
    assert_eq!(failures.len(), 4, "{failures:?}");
    assert!(
        failures
            .iter()
            .all(|e| e["fields"]["error_code"] == "FG-DB-0001"),
        "{failures:?}"
    );
}

/// Review fix 5: the hub's own sample of the NOTIFY queue logs
/// `FG-DB-0023` at its catalog severity while the queue is half full —
/// once while it lasts — and again for a new backlog after it drained.
#[test]
fn a_filling_notify_queue_is_logged_by_the_hub_while_it_lasts() {
    let logs = Captured::default();
    let _guard = logs.install();
    let rid = RequestId::generate();
    let mut backlog = Relog::default();
    sampled(&mut backlog, 0.0, &rid);
    sampled(&mut backlog, 0.499, &rid);
    assert!(logs.failures().is_empty(), "{:?}", logs.failures());
    sampled(&mut backlog, 0.6, &rid);
    sampled(&mut backlog, 0.7, &rid);
    let failures = logs.failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0]["fields"]["error_code"], "FG-DB-0023");
    assert_eq!(failures[0]["level"], "ERROR", "{failures:?}");
    let said = context(&failures[0]);
    for part in ["usage=0.600", "threshold=0.5", "operation=wake_listen"] {
        assert!(said.contains(part), "{part} in {said}");
    }
    // Drained, then filling again: a new backlog, logged at once.
    sampled(&mut backlog, 0.1, &rid);
    sampled(&mut backlog, 0.5, &rid);
    assert_eq!(logs.failures().len(), 2, "{:?}", logs.failures());
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

/// Plan 18 S3 (`forge_refs`): one statement announces every repository,
/// each id once, to the waiters of every process.
#[tokio::test]
async fn a_ref_change_is_announced_with_the_repository_ids() {
    let t = FreshDb::migrated().await;
    let hub = WakeHub::spawn(&t.db, &[Channel::Refs]).expect("hub");
    let mut rx = hub.subscribe_unseen(Channel::Refs);
    assert_eq!(rx.wait(SOON).await, Woken::Reconnected);
    refs_changed(&t.db, &[]).await.expect("nothing to announce");
    assert_eq!(rx.wait(QUIET).await, Woken::SafetyNet);
    let repos = [RepoId::new(7), RepoId::new(9), RepoId::new(7)];
    refs_changed(&t.db, &repos).await.expect("announce");
    let mut heard = Vec::new();
    while heard.len() < 2 {
        let woken = rx.wait(SOON).await;
        assert!(matches!(woken, Woken::Notified(_)), "{woken:?}");
        if let Woken::Notified(payloads) = woken {
            heard.extend(payloads);
        }
    }
    heard.sort();
    assert_eq!(heard, ["7", "9"]);
    assert_eq!(rx.wait(QUIET).await, Woken::SafetyNet, "each id once");
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
    let mut missed = Missed::default();
    assert_eq!(missed.note(at), Some(0));
    assert_eq!(missed.note(at), None);
    assert_eq!(missed.note(at + Duration::from_secs(59)), None);
    // The next logged one stands for the two that were not.
    assert_eq!(missed.note(at + MISSED_LOG_EVERY), Some(2));
    assert_eq!(missed.note(at + MISSED_LOG_EVERY * 2), Some(0));
}

/// Review fix 6: the logged `FG-DB-0024` carries `suppressed` — how many
/// misses of its channel the limiter did not log since the last one.
#[test]
fn a_logged_miss_carries_how_many_were_not_logged() {
    let logs = Captured::default();
    let _guard = logs.install();
    let (stop, _stopping) = watch::channel(false);
    let hub = WakeHub(Arc::new(Handle {
        shared: Arc::new(Shared::new(&[Channel::Jobs, Channel::Events])),
        stop,
        task: Mutex::new(None),
    }));
    let rid = RequestId::generate();
    let miss = || ForgeError::new(ErrorCode::WakeupMissed).with_context("kind", "test.a");
    assert!(hub.missed(Channel::Jobs, miss(), &rid));
    assert!(!hub.missed(Channel::Jobs, miss(), &rid));
    assert!(!hub.missed(Channel::Jobs, miss(), &rid));
    // Each channel has its own limiter and its own count.
    assert!(hub.missed(Channel::Events, miss(), &rid));
    // A minute after the logged one.
    {
        let mut limiters = hub.0.shared.missed.lock().expect("limiters");
        limiters[Channel::Jobs.index()].logged = Instant::now().checked_sub(MISSED_LOG_EVERY);
    }
    assert!(hub.missed(Channel::Jobs, miss(), &rid));
    let said: Vec<String> = logs.failures().iter().map(context).collect();
    assert_eq!(said.len(), 3, "{said:?}");
    for (event, parts) in [
        (0, ["channel=forge_jobs", "suppressed=0"]),
        (1, ["channel=forge_events_sequenced", "suppressed=0"]),
        (2, ["channel=forge_jobs", "suppressed=2"]),
    ] {
        for part in parts {
            assert!(said[event].contains(part), "{part} in {said:?}");
        }
    }
}
