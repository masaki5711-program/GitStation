//! Branch snapshots per repository (plan 18 S8, ADR-0037 (g)): which
//! branches exist, for the commands that only ask "is there such a
//! branch" — a merge request create or retarget — so that any number of
//! them on one repository share one `for-each-ref refs/heads/`.
//!
//! - **Names only.** A snapshot holds branch names and whether each names
//!   a commit, never a commit id: nothing that depends on a commit id
//!   (a merge request's head, mergeability, a merge) can be answered from
//!   it. Those read git (`browse` / `merge`).
//! - **Single flight.** At most one read per repository runs at a time.
//!   Callers that arrive while it runs wait for it; the caller that
//!   started it (the leader) runs it in its own request. A leader that is
//!   cancelled wakes its waiters, and one of them reads instead; a read
//!   that failed fails every caller waiting for it (its own error, never
//!   "missing", never "present") and is not kept.
//! - **Positives from the snapshot, negatives from git.** "The branch
//!   exists" is answered from a snapshot whose read began less than `ttl`
//!   ago. Any other answer is given only from a read that began *after*
//!   the caller arrived: a branch pushed after the snapshot is found, with
//!   no invalidation at all. Concurrent misses share that read.
//! - **Invalidation.** [`RefSnapshotCache::invalidate`] drops the
//!   repository's snapshot; a read in progress is not kept and not joined
//!   by later callers. It is called for every ref write this process
//!   knows of (`Browser::invalidate`: `forge serve`'s pushes and ref
//!   writes; plan 18 S2 adds the `forge_refs` listener for the worker's).
//! - **Staleness bound.** A ref write this process is not told about
//!   (another process, until S2) can only leave a *deleted* branch
//!   answered "present", and for at most `ttl` measured from the start of
//!   the read that saw it. The create that believed it records no commit
//!   id; its version job reads git and ends `source_missing`.
//!
//! No I/O here: the read is the caller's closure (`browse/branches.rs`).

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use forge_core::RepoId;
use forge_errors::ForgeError;
use tokio::sync::watch;
use tokio::time::Instant;

mod snapshot;

pub use snapshot::{BranchAnswer, BranchSnapshot};

/// What one read delivered to everyone waiting for it.
type Outcome = Result<Arc<BranchSnapshot>, Arc<ForgeError>>;

/// The snapshots (see the module docs). Cheap to clone (shared state).
#[derive(Debug, Clone)]
pub struct RefSnapshotCache {
    inner: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    ttl: Duration,
    max_repos: usize,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Reads started so far, over all repositories: a read's id. A caller
    /// notes it on arrival; a read with a greater id began after that.
    reads: u64,
    repos: HashMap<RepoId, RepoState>,
}

#[derive(Debug, Default)]
struct RepoState {
    /// Reads up to this id may predate an invalidation: not kept, and not
    /// joined by a caller that arrived after it.
    floor: u64,
    current: Option<Stored>,
    flight: Option<Flight>,
}

#[derive(Debug)]
struct Stored {
    read: u64,
    /// When its read began (the staleness bound counts from here).
    at: Instant,
    snapshot: Arc<BranchSnapshot>,
}

#[derive(Debug)]
struct Flight {
    read: u64,
    at: Instant,
    /// `None` until the read is over; closed with `None` when its leader
    /// went away.
    rx: watch::Receiver<Option<Outcome>>,
}

/// What a caller does next.
enum Step {
    /// Answer from this snapshot (of the read `read`).
    Have {
        snapshot: Arc<BranchSnapshot>,
        read: u64,
    },
    /// Wait for the read in progress. `usable`: its outcome may answer
    /// this caller; otherwise only its end is awaited (one read at a time).
    Wait {
        rx: watch::Receiver<Option<Outcome>>,
        read: u64,
        at: Instant,
        usable: bool,
    },
    /// Run the read.
    Lead(Lead),
}

/// The leader's side of a read. Dropped without [`Lead::finish`] (the
/// leader was cancelled or its read panicked): the read is unregistered
/// and its waiters wake with nothing, to read themselves.
struct Lead {
    shared: Arc<Shared>,
    repo: RepoId,
    read: u64,
    at: Instant,
    tx: watch::Sender<Option<Outcome>>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        // A poisoned lock: the state is plain data, every update is
        // complete before the guard drops.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The read `read` of `repo` is over: unregister it and, when
    /// `snapshot` is given and no invalidation happened since it began,
    /// keep it.
    fn settle(&self, repo: RepoId, read: u64, at: Instant, snapshot: Option<&Arc<BranchSnapshot>>) {
        let mut state = self.state();
        let Some(entry) = state.repos.get_mut(&repo) else {
            return;
        };
        if entry.flight.as_ref().is_some_and(|f| f.read == read) {
            entry.flight = None;
        }
        if let Some(snapshot) = snapshot {
            let newer = entry.current.as_ref().is_none_or(|c| c.read < read);
            if read > entry.floor && newer {
                entry.current = Some(Stored {
                    read,
                    at,
                    snapshot: Arc::clone(snapshot),
                });
            }
        }
        if entry.current.is_none() && entry.flight.is_none() {
            state.repos.remove(&repo);
        }
        self.evict(&mut state, repo);
    }

    /// Keep at most `max_repos` repositories: drop what expired, then the
    /// oldest snapshot. A repository with a read in progress, and `keep`,
    /// stay.
    fn evict(&self, state: &mut State, keep: RepoId) {
        let max = self.max_repos.max(1);
        if state.repos.len() <= max {
            return;
        }
        let ttl = self.ttl;
        state.repos.retain(|repo, entry| {
            *repo == keep
                || entry.flight.is_some()
                || entry.current.as_ref().is_some_and(|c| c.at.elapsed() < ttl)
        });
        while state.repos.len() > max {
            let oldest = state
                .repos
                .iter()
                .filter(|(repo, entry)| **repo != keep && entry.flight.is_none())
                .filter_map(|(repo, entry)| entry.current.as_ref().map(|c| (c.at, *repo)))
                .min_by_key(|(at, _)| *at);
            let Some((_, oldest)) = oldest else {
                return;
            };
            state.repos.remove(&oldest);
        }
    }
}

impl Lead {
    /// Publish the read's outcome to its waiters, keep a snapshot, and
    /// hand the leader its own copy.
    fn finish(
        self,
        result: Result<BranchSnapshot, ForgeError>,
    ) -> Result<Arc<BranchSnapshot>, ForgeError> {
        let outcome: Outcome = result.map(Arc::new).map_err(Arc::new);
        self.shared
            .settle(self.repo, self.read, self.at, outcome.as_ref().ok());
        // No receiver left is fine: nobody waited.
        // clone: the waiters' copy (an `Arc` either way).
        let _before = self.tx.send_replace(Some(outcome.clone()));
        outcome.map_err(|e| shared_error(&e))
        // `self` drops here: its read is already unregistered.
    }
}

impl Drop for Lead {
    fn drop(&mut self) {
        // After `finish` this finds nothing to do; without it, it is what
        // wakes the waiters (the sender closes with no outcome).
        if self.tx.borrow().is_none() {
            self.shared.settle(self.repo, self.read, self.at, None);
        }
    }
}

impl RefSnapshotCache {
    /// `ttl`: how long a snapshot answers "exists" (from the start of its
    /// read). `max_repos`: repositories kept at once.
    pub fn new(ttl: Duration, max_repos: usize) -> Self {
        Self {
            inner: Arc::new(Shared {
                ttl,
                max_repos,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Drop `repo`'s snapshot: its refs changed. A read in progress is not
    /// kept, and a caller that arrives from now on does not take its
    /// answer.
    pub fn invalidate(&self, repo: RepoId) {
        let mut state = self.inner.state();
        let reads = state.reads;
        let Some(entry) = state.repos.get_mut(&repo) else {
            return;
        };
        entry.current = None;
        if entry.flight.is_some() {
            entry.floor = reads;
        } else {
            state.repos.remove(&repo);
        }
    }

    /// What is known about the branch `full` (a full `refs/heads/…` name)
    /// of `repo`. `load` reads the repository's branches; it runs at most
    /// once per call, in this future, and only when this caller is the one
    /// to read (see the module docs). `Err`: the read this caller ran or
    /// waited for failed (that read's error).
    ///
    /// The caller bounds the wait (its operation's deadline): a waiter
    /// waits as long as the read it joined takes.
    pub async fn branch<L, F>(
        &self,
        repo: RepoId,
        full: &str,
        load: L,
    ) -> Result<BranchAnswer, ForgeError>
    where
        L: Fn() -> F,
        F: Future<Output = Result<BranchSnapshot, ForgeError>>,
    {
        let arrival = self.inner.state().reads;
        // Once a snapshot said anything but "exists": only a read that
        // began after `arrival` answers.
        let mut confirm = false;
        loop {
            match self.step(repo, arrival, confirm) {
                Step::Have { snapshot, read } => {
                    let answer = snapshot.answer(full);
                    if is_final(answer, read > arrival) {
                        return Ok(answer);
                    }
                    confirm = true;
                }
                Step::Wait {
                    mut rx,
                    read,
                    at,
                    usable,
                } => {
                    let outcome = settled(&mut rx).await;
                    if !usable {
                        continue;
                    }
                    let snapshot = match outcome {
                        // Its leader went away: read (or wait) again.
                        None => continue,
                        Some(Err(e)) => return Err(shared_error(&e)),
                        Some(Ok(snapshot)) => snapshot,
                    };
                    let after = read > arrival;
                    if !after && at.elapsed() >= self.inner.ttl {
                        // A read that began before this caller arrived and
                        // took longer than a snapshot may be reused.
                        confirm = true;
                        continue;
                    }
                    let answer = snapshot.answer(full);
                    if is_final(answer, after) {
                        return Ok(answer);
                    }
                    confirm = true;
                }
                Step::Lead(lead) => {
                    // This read began after `arrival`: whatever it says is
                    // the answer.
                    let result = load().await;
                    return lead.finish(result).map(|snapshot| snapshot.answer(full));
                }
            }
        }
    }

    fn step(&self, repo: RepoId, arrival: u64, confirm: bool) -> Step {
        let mut state = self.inner.state();
        let State { reads, repos } = &mut *state;
        let entry = repos.entry(repo).or_default();
        if let Some(current) = &entry.current {
            let fresh = current.at.elapsed() < self.inner.ttl;
            if current.read > arrival || (!confirm && fresh) {
                return Step::Have {
                    snapshot: Arc::clone(&current.snapshot),
                    read: current.read,
                };
            }
        }
        if let Some(flight) = &entry.flight {
            return Step::Wait {
                rx: flight.rx.clone(), // clone: one receiver per waiter
                read: flight.read,
                at: flight.at,
                usable: flight.read > arrival || (!confirm && flight.read > entry.floor),
            };
        }
        *reads += 1;
        let read = *reads;
        let at = Instant::now();
        let (tx, rx) = watch::channel(None);
        entry.flight = Some(Flight { read, at, rx });
        Step::Lead(Lead {
            shared: Arc::clone(&self.inner),
            repo,
            read,
            at,
            tx,
        })
    }

    /// Repositories with a snapshot or a read in progress (tests).
    #[cfg(test)]
    fn repos(&self) -> usize {
        self.inner.state().repos.len()
    }
}

/// "Exists" is final from any usable snapshot; "ask git for that name" is
/// final too (the caller's exact query is the confirmation); anything else
/// only from a read that began after the caller arrived.
fn is_final(answer: BranchAnswer, after_arrival: bool) -> bool {
    match answer {
        BranchAnswer::Commit | BranchAnswer::Unknown => true,
        BranchAnswer::NotACommit | BranchAnswer::Missing => after_arrival,
    }
}

/// The read's outcome, once there is one; `None` when its leader went away
/// without one.
async fn settled(rx: &mut watch::Receiver<Option<Outcome>>) -> Option<Outcome> {
    match rx.wait_for(Option::is_some).await {
        Ok(outcome) => outcome.clone(), // clone: out of the channel's lock
        Err(_closed) => None,
    }
}

/// One caller's copy of a read's error: the same code and context, and
/// the original's cause chain for the log.
fn shared_error(original: &Arc<ForgeError>) -> ForgeError {
    let mut copy = ForgeError::new(original.code());
    for &(key, ref value) in original.context() {
        copy = copy.with_context(key, value);
    }
    if StdError::source(original.as_ref()).is_some() {
        copy = copy.with_source(SharedCause(Arc::clone(original)));
    }
    copy
}

/// The cause of a shared error: displays as, and chains to, the original
/// error's own cause.
#[derive(Debug)]
struct SharedCause(Arc<ForgeError>);

impl fmt::Display for SharedCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match StdError::source(self.0.as_ref()) {
            Some(cause) => write!(f, "{cause}"),
            None => Ok(()),
        }
    }
}

impl StdError for SharedCause {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        StdError::source(self.0.as_ref()).and_then(|cause| cause.source())
    }
}

#[cfg(test)]
#[path = "refs_cache_tests.rs"]
mod tests;
