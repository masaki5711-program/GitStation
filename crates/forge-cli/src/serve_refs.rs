//! `forge_refs` in `forge serve` (plan 18 S3, ADR-0037 (a) and its
//! `forge_refs` addendum): the ref writes of this process are announced
//! to the other processes, and what they announce drops this process's
//! ref caches.
//!
//! Every ref writer of `forge serve` — pushes, the web commit, merges, the
//! merge queue, reverts, the autofix, version refs — calls
//! [`Browser::invalidate`] after its write. The browser tells this module
//! ([`Browser::on_refs_changed`]), which only notes the repository: the
//! writer never waits for the database. The **announcer** task sends the
//! noted repositories in one statement (`forge_db::wake::refs_changed`,
//! payload: the repository id). The **listener** task waits on the wake
//! hub and calls [`Browser::forget_refs`] for each repository it hears
//! of: the caches are dropped and nothing is announced again.
//!
//! A notification is a hint, never the truth. The caches it drops age out
//! by themselves after `refs_ttl` (5 s), so:
//!
//! - an announcement that fails is logged with its own code and
//!   `task=refs_announce` (the first of a streak, then at most once a
//!   minute with the count) and is NOT sent again;
//! - a lost notification, a listener that is down or a waiter that fell
//!   behind costs at most `refs_ttl` — the listener does nothing on a
//!   reconnect;
//! - a payload that is not a repository id is ignored (debug line).
//!
//! The process hears its own announcements too and reads those refs once
//! more: one `for-each-ref`, never a wrong answer.
//!
//! Both tasks run under the supervisor of `serve_merges` (an exit nobody
//! asked for is `FG-SYS-0005 operation=merge_task_exited` with
//! `task=refs_announce` / `task=refs_listen`, and the task is started
//! again). At shutdown, after every ref writer has stopped, the announcer
//! sends what is still noted, then both end — before the hub and the
//! pools are closed.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use forge_core::{telemetry, RepoId, RequestId};
use forge_db::wake::{Channel, WakeHub, Woken};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::serve_merges::supervise;

/// How long a shutdown waits for the two tasks (the last announcement is
/// one bounded statement; the listener ends at once).
pub(crate) const STOP_WAIT: Duration = Duration::from_secs(5);
/// The backoff of a restarted task.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A failing announcement is logged at most this often after its first
/// line.
const LOG_EVERY: Duration = Duration::from_secs(60);
/// The listener's own period when it hears nothing. It has nothing to
/// look at by itself (the caches' `refs_ttl` is the safety net): the
/// period only bounds the wait.
const LISTEN_IDLE: Duration = Duration::from_secs(60);

/// The repositories whose refs this process changed and has not announced
/// yet. Bounded by the number of repositories (ids, coalesced).
#[derive(Default)]
struct Noted {
    repos: Mutex<BTreeSet<RepoId>>,
    added: Notify,
}

impl Noted {
    /// On the writer's path: no I/O and no await.
    fn note(&self, repo: RepoId) {
        self.repos
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(repo);
        // One stored permit is enough: the announcer takes everything.
        self.added.notify_one();
    }

    fn take(&self) -> Vec<RepoId> {
        let mut repos = self.repos.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *repos).into_iter().collect()
    }
}

/// The supervised announcer and listener; [`RefsTasks::stop`] ends them.
pub(crate) struct RefsTasks {
    stop: watch::Sender<bool>,
    supervisors: Vec<(&'static str, JoinHandle<()>)>,
}

impl RefsTasks {
    /// Stop both tasks and wait at most `wait` for them. Call it after the
    /// ref writers have stopped: the announcer sends what they noted
    /// before it ends. A task that panicked or did not end in time is
    /// logged, never ignored.
    pub(crate) async fn stop(self, wait: Duration) {
        // No receiver left means every task already ended.
        let _ = self.stop.send(true);
        let deadline = tokio::time::Instant::now() + wait;
        for (task, supervisor) in self.supervisors {
            match tokio::time::timeout_at(deadline, supervisor).await {
                Ok(Ok(())) => {}
                Ok(Err(join)) => telemetry::log_failure(
                    &ForgeError::new(ErrorCode::ServeFailed)
                        .with_context("operation", "refs_supervisor_exited")
                        .with_context("task", task)
                        .with_context("panicked", join.is_panic()),
                    &RequestId::generate(),
                ),
                Err(_) => telemetry::log_failure(
                    &ForgeError::new(ErrorCode::ServeFailed)
                        .with_context("operation", "refs_task_pending")
                        .with_context("task", task)
                        .with_context("wait_ms", wait.as_millis()),
                    &RequestId::generate(),
                ),
            }
        }
    }
}

/// Announce the ref writes made through `browser` (and its clones) and
/// drop its caches for the ref writes other processes announce on `hub`.
pub(crate) fn spawn(db: &Db, hub: &WakeHub, browser: &Browser) -> RefsTasks {
    let noted = Arc::new(Noted::default());
    let sink = Arc::clone(&noted);
    if !browser.on_refs_changed(move |repo| sink.note(repo)) {
        // A programming error (two announcers on one browser): the first
        // one keeps announcing, this one only listens.
        telemetry::log_failure(
            &ForgeError::new(ErrorCode::ServeFailed).with_context("operation", "refs_hook_taken"),
            &RequestId::generate(),
        );
    }
    let (stop, stopped) = watch::channel(false);
    let backoff = (BACKOFF_MIN, BACKOFF_MAX);
    let announcer = {
        let db = db.clone(); // clone: pool handles
        tokio::spawn(supervise(
            "refs_announce",
            stopped.clone(), // clone: the supervisor's own receiver
            backoff,
            // clone: a restarted task gets its own handles; the noted set
            // is shared, so nothing noted meanwhile is lost
            move |s| tokio::spawn(announce(db.clone(), Arc::clone(&noted), s)),
        ))
    };
    let listener = {
        let hub = hub.clone(); // clone: the hub's handle
        let browser = browser.clone(); // clone: shares the ref caches
        tokio::spawn(supervise("refs_listen", stopped, backoff, move |s| {
            // clone: a restarted task subscribes again
            tokio::spawn(listen(hub.clone(), browser.clone(), s))
        }))
    };
    tracing::info!("forge.serve.refs_started");
    RefsTasks {
        stop,
        supervisors: vec![("refs_announce", announcer), ("refs_listen", listener)],
    }
}

/// A streak of failed announcements: logged at its first failure, then at
/// most every [`LOG_EVERY`] with its count.
#[derive(Default)]
struct Streak {
    count: u64,
    logged_at: Option<tokio::time::Instant>,
}

impl Streak {
    fn fail(&mut self, e: ForgeError, repos: usize) {
        self.count += 1;
        if self.logged_at.is_none_or(|at| at.elapsed() >= LOG_EVERY) {
            telemetry::log_failure(
                &e.with_context("task", "refs_announce")
                    .with_context("repos", repos)
                    .with_context("streak", self.count),
                &RequestId::generate(),
            );
            self.logged_at = Some(tokio::time::Instant::now());
        }
    }
}

/// Send what was noted, as it is noted; once more when stopping.
async fn announce(db: Db, noted: Arc<Noted>, mut stop: watch::Receiver<bool>) {
    let mut streak = Streak::default();
    loop {
        let stopping = tokio::select! {
            () = noted.added.notified() => false,
            // Stopping (or serve is gone): send what is noted, then end.
            _ = stop.wait_for(|s| *s) => true,
        };
        let repos = noted.take();
        if !repos.is_empty() {
            // Bounded by the database handle (`Db::within`). Not sent
            // again on failure: the listeners' caches age out by
            // themselves.
            match forge_db::wake::refs_changed(&db, &repos).await {
                Ok(()) => streak = Streak::default(),
                Err(e) => streak.fail(e, repos.len()),
            }
        }
        if stopping {
            break;
        }
    }
    tracing::info!("forge.serve.refs_announce_stopped");
}

/// Drop the caches of every repository another process (or this one)
/// announced.
async fn listen(hub: WakeHub, browser: Browser, mut stop: watch::Receiver<bool>) {
    let mut rx = hub.subscribe(Channel::Refs);
    loop {
        let woken = tokio::select! {
            woken = rx.wait(LISTEN_IDLE) => woken,
            _ = stop.wait_for(|s| *s) => break,
        };
        match woken {
            Woken::Notified(payloads) => {
                for payload in payloads {
                    match payload.parse::<i64>() {
                        Ok(id) => browser.forget_refs(RepoId::new(id)),
                        // Not ours (anyone may NOTIFY): nothing to drop.
                        Err(_) => tracing::debug!(
                            payload_bytes = payload.len(),
                            "forge.serve.refs_payload_ignored"
                        ),
                    }
                }
            }
            // Nothing heard, or notifications may have been missed: the
            // caches' own `refs_ttl` covers both.
            Woken::SafetyNet | Woken::Reconnected => {}
        }
    }
    tracing::info!("forge.serve.refs_listen_stopped");
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use forge_core::RefName;
    use forge_db::test_support::FreshDb;
    use forge_git::browse::BrowseLimits;

    use super::*;
    use crate::backup::tests::{commit, git};
    use crate::worker::provision::tests::storage;
    use crate::worker::provision::Storage;

    /// Far longer than a notification takes on the test server, far
    /// shorter than the `refs_ttl` of these tests.
    const SOON: Duration = Duration::from_secs(30);
    const REFS_TTL: Duration = Duration::from_secs(3600);

    fn branch(short: &str) -> RefName {
        RefName::parse(&format!("refs/heads/{short}")).expect("branch")
    }

    /// What one `forge serve` process has of its own: a LISTEN connection
    /// (its hub) and a browser whose snapshot never ages out during the
    /// test — only a signal can change an answer.
    async fn process(t: &FreshDb, storage: &Storage) -> (WakeHub, Browser) {
        let hub = WakeHub::spawn(&t.db, &Channel::ALL).expect("hub");
        let began = std::time::Instant::now();
        while !hub.is_live(Channel::Refs) {
            assert!(began.elapsed() < SOON, "the listener never came up");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let browser = Browser::new(
            storage.git.clone(),  // clone: handles
            storage.root.clone(), // clone: the root path
            BrowseLimits {
                refs_ttl: REFS_TTL,
                ..BrowseLimits::default()
            },
        );
        (hub, browser)
    }

    /// Whether `browser` answers "missing" for `name`.
    async fn missing(browser: &Browser, repo: RepoId, name: &str) -> bool {
        match browser.require_branch(repo, &branch(name)).await {
            Ok(()) => false,
            Err(e) => {
                assert_eq!(e.code(), ErrorCode::RepositoryPathNotResolved, "{e:?}");
                true
            }
        }
    }

    /// `browser` answers "missing" for `name` within [`SOON`].
    async fn seen_missing(browser: &Browser, repo: RepoId, name: &str, what: &str) {
        let began = std::time::Instant::now();
        while !missing(browser, repo, name).await {
            assert!(
                began.elapsed() < SOON,
                "{what}: still answered from the snapshot"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[test]
    fn noted_repositories_coalesce_and_are_taken_once() {
        let noted = Noted::default();
        for id in [7, 3, 7, 7] {
            noted.note(RepoId::new(id));
        }
        assert_eq!(noted.take(), vec![RepoId::new(3), RepoId::new(7)]);
        assert!(noted.take().is_empty());
    }

    /// Two processes on one database and one storage root. A branch the
    /// writer's process deletes stays "present" for the reader while
    /// nobody says so (its snapshot lives for an hour here); as soon as
    /// the writer invalidates — what every ref writer does — the reader
    /// answers from git, without waiting for `refs_ttl`. A write right
    /// before the writer's shutdown is announced by the stop.
    #[tokio::test]
    async fn a_ref_change_of_another_process_is_seen_without_waiting_for_refs_ttl() {
        let t = FreshDb::migrated().await;
        let (_dir, storage) = storage();
        let repo = RepoId::new(11);
        let path = storage.root.path_for(repo);
        std::fs::create_dir_all(&path).expect("repository directory");
        let init = Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&path)
            .status()
            .expect("git init");
        assert!(init.success(), "git init --bare");
        for name in ["main", "old", "last"] {
            commit(&path, name, "synthetic");
        }

        let (writer_hub, writer) = process(&t, &storage).await;
        let (reader_hub, reader) = process(&t, &storage).await;
        let writing = spawn(&t.db, &writer_hub, &writer);
        let reading = spawn(&t.db, &reader_hub, &reader);
        assert!(!missing(&reader, repo, "old").await, "the snapshot is read");

        // Deleted, and nobody said so: a stale positive within refs_ttl.
        // A payload that is no repository id changes nothing either.
        git(&path, &["update-ref", "-d", "refs/heads/old"]);
        reader_hub.nudge(Channel::Refs, "not a repository id");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!missing(&reader, repo, "old").await, "not told yet");

        // The writer's process says so: the reader drops its snapshot.
        writer.invalidate(repo);
        seen_missing(&reader, repo, "old", "announced").await;
        assert!(!missing(&reader, repo, "last").await, "read again");

        // A write right before the shutdown: the stop announces it.
        git(&path, &["update-ref", "-d", "refs/heads/last"]);
        writer.invalidate(repo);
        writing.stop(SOON).await;
        seen_missing(&reader, repo, "last", "announced by the stop").await;

        reading.stop(SOON).await;
        writer_hub.stop(SOON).await.expect("stop the writer's hub");
        reader_hub.stop(SOON).await.expect("stop the reader's hub");
        t.drop_now().await;
    }
}
