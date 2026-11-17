#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! The keyed cap with a bounded wait (`gate.rs`).

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use super::{KeyedGate, Refusal};

fn ctx(err: &ForgeError, key: &str) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

async fn until_users(gate: &KeyedGate<String>, key: &str, n: usize) {
    for _ in 0..200 {
        if gate.users(&key.to_owned()) == n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never {n} users of {key}");
}

#[tokio::test]
async fn a_waiter_gets_the_slot_its_holder_lets_go() {
    let gate = KeyedGate::<String>::new(1, 4);
    let key = "user:1".to_owned();
    let held = gate.acquire(&key, Duration::ZERO).await.expect("free");
    let waiter = {
        let gate = gate.clone(); // clone: the task's handle
        let key = key.clone(); // clone: the task's key
        tokio::spawn(async move { gate.acquire(&key, Duration::from_secs(10)).await.map(drop) })
    };
    until_users(&gate, &key, 2).await;
    assert!(!waiter.is_finished(), "waits while the slot is held");
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("served")
        .expect("join")
        .expect("the freed slot");
    assert_eq!(gate.users(&key), 0, "the entry is gone");
}

#[tokio::test]
async fn a_wait_that_runs_out_is_a_timeout_and_gives_the_place_back() {
    let gate = KeyedGate::<String>::new(1, 4);
    let key = "user:1".to_owned();
    let _held = gate.acquire(&key, Duration::ZERO).await.expect("free");
    let started = std::time::Instant::now();
    let refusal = gate
        .acquire(&key, Duration::from_millis(80))
        .await
        .expect_err("busy");
    assert!(started.elapsed() >= Duration::from_millis(80), "it waited");
    assert!(matches!(refusal, Refusal::Timeout(w) if w == Duration::from_millis(80)));
    assert_eq!(gate.users(&key), 1);
    let e = refusal.into_error(ForgeError::new(ErrorCode::GitBusy), 4);
    assert_eq!(e.code(), ErrorCode::GitBusy);
    assert_eq!(ctx(&e, "queue"), "timeout");
    assert_eq!(ctx(&e, "waited_ms"), "80");
    assert_eq!(ctx(&e, crate::RETRY_AFTER_CONTEXT), "1");
}

#[tokio::test]
async fn a_full_queue_is_refused_at_once_and_other_keys_are_free() {
    let gate = KeyedGate::<String>::new(1, 1);
    let key = "user:1".to_owned();
    let _held = gate.acquire(&key, Duration::ZERO).await.expect("free");
    let waiter = {
        let gate = gate.clone(); // clone: the task's handle
        let key = key.clone(); // clone: the task's key
        tokio::spawn(async move { gate.acquire(&key, Duration::from_secs(10)).await.map(drop) })
    };
    until_users(&gate, &key, 2).await;
    let started = std::time::Instant::now();
    let refusal = gate
        .acquire(&key, Duration::from_secs(10))
        .await
        .expect_err("full");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "refused at once"
    );
    assert!(matches!(refusal, Refusal::QueueFull));
    let e = refusal.into_error(ForgeError::new(ErrorCode::GitBusy), 1);
    assert_eq!(ctx(&e, "queue"), "full");
    assert!(gate
        .acquire(&"user:2".to_owned(), Duration::ZERO)
        .await
        .is_ok());
    waiter.abort();
    let _ = waiter.await; // ignore: aborted
    assert_eq!(
        gate.users(&key),
        1,
        "an aborted waiter gives its place back"
    );
}

#[tokio::test]
async fn waiters_are_served_in_arrival_order() {
    let gate = KeyedGate::<String>::new(1, 8);
    let key = "user:1".to_owned();
    let held = gate.acquire(&key, Duration::ZERO).await.expect("free");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = Vec::new();
    for i in 0..4 {
        let task_gate = gate.clone(); // clone: the task's handle
        let task_key = key.clone(); // clone: the task's key
        let tx = tx.clone(); // clone: one sender per task
        tasks.push(tokio::spawn(async move {
            let permit = task_gate.acquire(&task_key, Duration::from_secs(10)).await;
            let _ = tx.send(i); // ignore: the receiver outlives the tasks
            drop(permit);
        }));
        until_users(&gate, &key, i + 2).await;
    }
    drop(held);
    let mut order = Vec::new();
    for _ in 0..4 {
        order.push(
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("served")
                .expect("sent"),
        );
    }
    assert_eq!(order, vec![0, 1, 2, 3]);
    for task in tasks {
        task.await.expect("join");
    }
    assert_eq!(gate.held(&key), 0);
}
