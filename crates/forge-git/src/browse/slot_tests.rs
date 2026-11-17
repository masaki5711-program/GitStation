#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! The per-repository browse cap waits in a bounded queue before it
//! refuses (load test 2026-11-17 F1: nine merge requests opened at once on
//! one repository were refused at once with `FG-GIT-0004 repo_browse`).

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use super::tests::{fixture, reason};
use super::{BrowseLimits, Deadline};

fn ctx(err: &ForgeError, key: &str) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// The load test's shape: more callers than slots, all served (each waits
/// its turn) instead of eight refused.
#[tokio::test]
async fn more_browse_calls_than_the_repository_cap_wait_and_all_succeed() {
    let f = fixture().await;
    let b = f.browser(BrowseLimits {
        max_per_repo: 1,
        // Every call loads (no cached ref list hides the cap).
        refs_ttl: Duration::ZERO,
        ..BrowseLimits::default()
    });
    let calls: Vec<_> = (0..9)
        .map(|_| {
            let b = b.clone(); // clone: one handle per task
            let id = f.id;
            tokio::spawn(async move { b.refs(id).await })
        })
        .collect();
    for call in calls {
        call.await
            .expect("join")
            .expect("served after a short wait");
    }
    assert_eq!(b.users(f.id), 0, "every place given back");
}

/// A waiter gets the slot the moment its holder lets go.
#[tokio::test]
async fn a_call_over_the_repository_cap_gets_the_next_free_slot() {
    let f = fixture().await;
    let b = f.browser(BrowseLimits {
        max_per_repo: 1,
        repo_wait: Duration::from_secs(10),
        ..BrowseLimits::default()
    });
    let held = b
        .claim(f.id, Deadline::new(Duration::from_secs(10)))
        .await
        .expect("the only slot");
    let waiter = {
        let b = b.clone(); // clone: the task's handle
        let id = f.id;
        tokio::spawn(async move {
            b.claim(id, Deadline::new(Duration::from_secs(10)))
                .await
                .map(drop)
        })
    };
    for _ in 0..100 {
        if b.users(f.id) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(b.users(f.id), 2, "one holds, one waits");
    assert_eq!(b.held(f.id), 1, "the cap is unchanged");
    assert!(!waiter.is_finished(), "waits while the slot is held");
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("served")
        .expect("join")
        .expect("the freed slot");
    assert_eq!(b.users(f.id), 0);
}

/// A wait that runs out is `FG-GIT-0004 repo_browse` (`queue=timeout`),
/// with a retry hint, and gives its place back.
#[tokio::test]
async fn a_wait_for_a_repository_slot_that_runs_out_is_git_busy() {
    let f = fixture().await;
    let b = f.browser(BrowseLimits {
        max_per_repo: 1,
        repo_wait: Duration::from_millis(100),
        ..BrowseLimits::default()
    });
    let _held = b
        .claim(f.id, Deadline::new(Duration::from_secs(10)))
        .await
        .expect("the only slot");
    let started = std::time::Instant::now();
    let e = b
        .claim(f.id, Deadline::new(Duration::from_secs(10)))
        .await
        .expect_err("busy");
    assert!(started.elapsed() >= Duration::from_millis(100), "it waited");
    assert_eq!(e.code(), ErrorCode::GitBusy);
    assert_eq!(reason(&e), "repo_browse");
    assert_eq!(ctx(&e, "queue"), "timeout");
    assert_eq!(ctx(&e, "waited_ms"), "100");
    assert_eq!(ctx(&e, crate::RETRY_AFTER_CONTEXT), "1");
    assert_eq!(b.users(f.id), 1, "only the holder is left");
}

/// The wait never outlasts the operation's own deadline.
#[tokio::test]
async fn a_repository_slot_wait_ends_at_the_operation_deadline() {
    let f = fixture().await;
    let b = f.browser(BrowseLimits {
        max_per_repo: 1,
        repo_wait: Duration::from_secs(30),
        ..BrowseLimits::default()
    });
    let _held = b
        .claim(f.id, Deadline::new(Duration::from_secs(30)))
        .await
        .expect("the only slot");
    let started = std::time::Instant::now();
    let e = b
        .claim(f.id, Deadline::new(Duration::from_millis(150)))
        .await
        .expect_err("busy");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "bounded by 150 ms"
    );
    assert_eq!(e.code(), ErrorCode::GitBusy);
    assert_eq!(ctx(&e, "queue"), "timeout");
}

/// The queue is bounded: one waiter more than `max_repo_waiters` is
/// refused at once (`queue=full`); other repositories are not affected.
#[tokio::test]
async fn a_full_repository_queue_refuses_at_once() {
    let f = fixture().await;
    let b = f.browser(BrowseLimits {
        max_per_repo: 1,
        max_repo_waiters: 1,
        repo_wait: Duration::from_secs(10),
        ..BrowseLimits::default()
    });
    let _held = b
        .claim(f.id, Deadline::new(Duration::from_secs(10)))
        .await
        .expect("the only slot");
    let waiter = {
        let b = b.clone(); // clone: the task's handle
        let id = f.id;
        tokio::spawn(async move {
            b.claim(id, Deadline::new(Duration::from_secs(10)))
                .await
                .map(drop)
        })
    };
    for _ in 0..100 {
        if b.users(f.id) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let started = std::time::Instant::now();
    let e = b
        .claim(f.id, Deadline::new(Duration::from_secs(10)))
        .await
        .expect_err("queue full");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "refused at once"
    );
    assert_eq!(e.code(), ErrorCode::GitBusy);
    assert_eq!(reason(&e), "repo_browse");
    assert_eq!(ctx(&e, "queue"), "full");
    assert!(b
        .claim(
            forge_core::RepoId::new(12),
            Deadline::new(Duration::from_secs(1))
        )
        .await
        .is_ok());
    waiter.abort();
    let _ = waiter.await; // ignore: aborted
    assert_eq!(b.users(f.id), 1, "the aborted waiter gave its place back");
}

/// `repo_wait` zero keeps the old answer: refused at once.
#[tokio::test]
async fn a_zero_repository_wait_refuses_at_once() {
    let f = fixture().await;
    let b = f.browser(BrowseLimits {
        max_per_repo: 1,
        repo_wait: Duration::ZERO,
        ..BrowseLimits::default()
    });
    let _held = b
        .claim(f.id, Deadline::new(Duration::from_secs(10)))
        .await
        .expect("the only slot");
    let e = b
        .claim(f.id, Deadline::new(Duration::from_secs(10)))
        .await
        .expect_err("busy");
    assert_eq!(ctx(&e, "queue"), "timeout");
    assert_eq!(ctx(&e, "waited_ms"), "0");
}
