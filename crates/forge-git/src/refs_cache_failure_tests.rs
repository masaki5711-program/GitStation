#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! A read that fails (`refs_cache/failure.rs`): which failures reach who
//! waited for the read, and which stay with the caller that ran it. On
//! tokio's paused clock, with the scripted read of `refs_cache_tests.rs`.

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::{ask, ask_in, asking, parked, repo, Fail, FakeGit, FAILS_AFTER, TTL};
use crate::refs_cache::{BranchAnswer, RefSnapshotCache};

/// A caller with a deadline of its own, as `Browser::require_branch` is
/// one: its own `FG-GIT-0002` when no answer came within `limit`.
fn asking_within(
    cache: &RefSnapshotCache,
    git: &FakeGit,
    short: &'static str,
    limit: Duration,
) -> JoinHandle<Result<BranchAnswer, ForgeError>> {
    let cache = cache.clone(); // clone: the task's handle
    let git = git.clone(); // clone: the task's handle
    tokio::spawn(async move {
        let asked = ask_in(&cache, &git, repo(1), short);
        match tokio::time::timeout(limit, asked).await {
            Ok(answer) => answer,
            Err(_elapsed) => Err(ForgeError::new(ErrorCode::GitTimeout)
                .with_context("operation", "browse")
                .with_context("timeout_ms", limit.as_millis())),
        }
    })
}

/// A failure that is the same for whoever reads (here: git cannot be
/// started) is that read's error for its leader and every waiter.
#[tokio::test(start_paused = true)]
async fn a_failed_read_fails_everyone_waiting_and_is_not_kept() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main"]);
    git.fail(Fail::Always(ErrorCode::GitUnavailable));
    let asks: Vec<_> = (0..5).map(|_| asking(&cache, &git, "main")).collect();
    parked().await;
    for ask in asks {
        let err = ask.await.unwrap().expect_err("the read failed");
        assert_eq!(err.code(), ErrorCode::GitUnavailable);
        assert_eq!(
            err.to_string(),
            "FG-GIT-0001 reason=synthetic: synthetic cause",
            "the read's own error, with its cause, for the leader and every waiter"
        );
    }
    assert_eq!(git.reads(), 1);
    assert_eq!(cache.repos(), 0, "a failure is not kept");

    git.fail(Fail::Never);
    assert_eq!(ask(&cache, &git, "main").await, BranchAnswer::Commit);
    assert_eq!(git.reads(), 2, "the next question reads again");
}

/// A reader refused for its place in a queue (`FG-GIT-0004`) or out of its
/// own time (`FG-GIT-0002`) fails alone: who waited for its read still has
/// time, and one of them reads instead.
#[tokio::test(start_paused = true)]
async fn a_refused_or_timed_out_leader_does_not_fail_a_waiter_that_still_has_time() {
    for code in [ErrorCode::GitBusy, ErrorCode::GitTimeout] {
        let cache = RefSnapshotCache::new(TTL, 16);
        let git = FakeGit::with(&["main"]);
        git.fail(Fail::Once(code));
        let leader = asking(&cache, &git, "main");
        parked().await;
        let waiters: Vec<_> = (0..5).map(|_| asking(&cache, &git, "main")).collect();
        parked().await;
        assert_eq!(git.reads(), 1, "they wait for the leader's read");

        let err = leader.await.unwrap().expect_err("the leader's read failed");
        assert_eq!(err.code(), code, "its own error, to itself");
        assert_eq!(
            err.to_string(),
            format!("{} reason=synthetic: synthetic cause", code.id())
        );
        for waiter in waiters {
            let answer = waiter.await.unwrap();
            assert_eq!(
                answer.expect("a waiter does not inherit the leader's refusal"),
                BranchAnswer::Commit,
                "{code:?}: answered by the next read"
            );
        }
        assert_eq!(git.reads(), 2, "one waiter read, the others shared it");
        assert_eq!(ask(&cache, &git, "main").await, BranchAnswer::Commit);
        assert_eq!(git.reads(), 2, "and that snapshot is kept");
    }
}

/// A waiter that reads in the refused leader's place does so on its own
/// time: out of it, it gets its own error — not the leader's — and who
/// still has time reads after it.
#[tokio::test(start_paused = true)]
async fn a_waiter_whose_own_deadline_passes_gets_its_own_error() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main"]);
    git.hold();
    git.fail(Fail::Once(ErrorCode::GitBusy));
    let leader = asking(&cache, &git, "main");
    parked().await;
    let short = asking_within(&cache, &git, "main", Duration::from_secs(6));
    parked().await;
    assert_eq!(git.reads(), 1, "it waits for the leader's read");

    // 5 s: the leader is refused, and the waiter reads (held back).
    let err = leader.await.unwrap().expect_err("the leader is refused");
    assert_eq!(err.code(), ErrorCode::GitBusy);
    parked().await;
    assert_eq!(git.reads(), 2, "the waiter reads in the leader's place");
    let patient = asking_within(&cache, &git, "main", Duration::from_secs(60));
    parked().await;
    assert_eq!(git.reads(), 2, "one read at a time");

    // 6 s: the waiter's own deadline.
    let err = short.await.unwrap().expect_err("out of its own time");
    assert_eq!(
        err.to_string(),
        "FG-GIT-0002 operation=browse timeout_ms=6000",
        "its own deadline, not the leader's refusal"
    );
    parked().await;
    assert_eq!(git.reads(), 3, "who still has time reads after it");
    git.release();
    assert_eq!(patient.await.unwrap().unwrap(), BranchAnswer::Commit);
    assert_eq!(git.reads(), 3);
}

/// A repository whose every read is refused: each caller reads once, one
/// after the other (never two reads at once), and ends in the registered
/// error. Nobody waits for ever.
#[tokio::test(start_paused = true)]
async fn a_saturated_repository_refuses_each_caller_in_turn_and_nobody_hangs() {
    let cache = RefSnapshotCache::new(TTL, 16);
    let git = FakeGit::with(&["main"]);
    git.fail(Fail::Always(ErrorCode::GitBusy));
    let started = Instant::now();
    let asks: Vec<_> = (0..5).map(|_| asking(&cache, &git, "main")).collect();
    for ask in asks {
        let answer = tokio::time::timeout(Duration::from_secs(600), ask)
            .await
            .expect("no caller hangs")
            .unwrap();
        assert_eq!(answer.expect_err("refused").code(), ErrorCode::GitBusy);
    }
    assert_eq!(git.reads(), 5, "one read per caller");
    let took = started.elapsed();
    assert!(
        took >= FAILS_AFTER * 5 && took < FAILS_AFTER * 6,
        "one at a time: {took:?}"
    );
    assert_eq!(cache.repos(), 0, "nothing is kept");
}
