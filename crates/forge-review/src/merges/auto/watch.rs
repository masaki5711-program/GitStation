//! The auto-merge worker's wait (plan 18 S3, ADR-0037 (a)).
//!
//! Before plan 18 the worker looked at fifty merge requests every five
//! seconds. Now `forge serve` runs one [`AutoMergeWatch`], which waits for
//! the first of:
//!
//! - **`forge_events_sequenced`** — the outbox sequencer gave events their
//!   numbers. The worker reads the events after its cursor and decides
//!   only the merge requests they name
//!   (`forge_db::review::auto_merge::named_by_events`: a merge request's
//!   own events, and a `check.completed` on its head or its merge result);
//! - **`forge_auto_merge`** — a mergeability check answered "can be
//!   merged" for the merge requests in the payloads (no event says so
//!   unless a conflict went away);
//! - **the sweep** — every `auto_merge.safety_poll_ms`, and whenever the
//!   listener (re)connected: every merge request auto-merge acts on, the
//!   oldest look first. It is the safety net of a lost notification and
//!   the only path of what names no merge request (a project's
//!   `merge.auto` or emergency stop changed, the instance's stop, a
//!   `forge/doc-lint` wait that ran out).
//!
//! A wake is a hint: every decision reads the tables again
//! ([`AutoMerger::scan`]). At most [`DECIDE_AT_ONCE`] merge requests are
//! decided at once.
//!
//! **Listener down** (no hub, or one of the two channels is not up): the
//! worker polls as before plan 18 — [`AutoMerger::tick`] every
//! [`AutoMergeWatch::FALLBACK_POLL`] — and sweeps when the listener is
//! back.
//!
//! The sweep reports no `FG-DB-0024`: "ready" has no single time here (a
//! verdict is decided from a dozen facts), and a settings change is found
//! by the sweep by design.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use forge_core::auto_merge::Verdict;
use forge_core::{telemetry, RequestId};
use forge_db::review::auto_merge::{self as db, Candidate};
use forge_db::wake::{Channel, WakeRx, Woken};
use forge_errors::{ErrorCode, ForgeError};
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::{AutoMergeReport, AutoMerger};

/// Merge requests decided at once.
pub const DECIDE_AT_ONCE: usize = 4;
/// Events read per statement, and statements per wake (the events past
/// them are read at the next wake: the cursor stays behind them).
const EVENTS_PER_READ: i64 = 200;
const EVENT_READS_MAX: usize = 10;
/// Rounds of one sweep (`AutoMergeLimits::batch` merge requests each);
/// what is left is the next sweep's first (the oldest look first).
const SWEEP_ROUNDS_MAX: usize = 20;
/// The poll while no hub listens for this process.
const FALLBACK_POLL: Duration = Duration::from_secs(5);

impl AutoMerger {
    /// Decide `due`, at most [`DECIDE_AT_ONCE`] at once, each in its own
    /// task (dropping this future ends them). A failure of one merge
    /// request is logged (`FG-REVIEW-0028`) and counted; the others go on.
    async fn decide_at_once(&self, due: Vec<Candidate>) -> AutoMergeReport {
        let mut report = AutoMergeReport::default();
        let mut waiting = due.into_iter();
        let mut running: JoinSet<(RequestId, Result<Verdict, ForgeError>)> = JoinSet::new();
        let mut deciding: HashMap<tokio::task::Id, Candidate> = HashMap::new();
        loop {
            while running.len() < DECIDE_AT_ONCE {
                let Some(c) = waiting.next() else { break };
                let worker = self.clone(); // clone: handles (pools, git, caches)
                let asked = c.clone(); // clone: the task's own copy
                let task = running.spawn(async move {
                    let request_id = RequestId::generate();
                    let scanned = worker.scan(&asked, &request_id).await;
                    (request_id, scanned)
                });
                deciding.insert(task.id(), c);
            }
            match running.join_next_with_id().await {
                None => break,
                Some(Ok((id, (request_id, scanned)))) => {
                    if let Some(c) = deciding.remove(&id) {
                        report.count(&c, &request_id, scanned);
                    }
                }
                // The task panicked (it is never cancelled while this
                // future lives): that merge request is the sweep's.
                Some(Err(join)) => {
                    report.looked += 1;
                    report.failed += 1;
                    let c = deciding.remove(&join.id());
                    telemetry::log_failure(
                        &ForgeError::new(ErrorCode::AutoMergeScanFailed)
                            .with_context("cause_code", ErrorCode::Internal.id())
                            .with_context("reason", "scan_task_ended")
                            .with_context("panicked", join.is_panic())
                            .with_context(
                                "merge_request_id",
                                c.map_or(0, |c| c.merge_request.get()),
                            ),
                        &RequestId::generate(),
                    );
                }
            }
        }
        report
    }

    /// Decide the merge requests the sequenced events after `cursor` name
    /// (module docs) and move `cursor` past the events read. A failure to
    /// read them is returned; the cursor then stays where the failure was
    /// and the sweep covers what was not decided.
    pub async fn decide_events(&self, cursor: &mut i64) -> Result<AutoMergeReport, ForgeError> {
        let mut report = AutoMergeReport::default();
        for _ in 0..EVENT_READS_MAX {
            let named = db::named_by_events(&self.merger.db, *cursor, EVENTS_PER_READ).await?;
            // Past them before deciding: an event is read once, and a
            // merge request that could not be decided is the sweep's.
            *cursor = named.upto;
            report.add(&self.decide_at_once(named.candidates).await);
            if named.events < EVENTS_PER_READ {
                break;
            }
        }
        Ok(report)
    }

    /// Decide the merge requests `ids` that auto-merge acts on (the
    /// payloads of `forge_auto_merge`; an id it does not act on is
    /// nothing).
    pub async fn decide_named(&self, ids: &[i64]) -> Result<AutoMergeReport, ForgeError> {
        let due = db::named(&self.merger.db, ids).await?;
        Ok(self.decide_at_once(due).await)
    }

    /// Look at every merge request auto-merge acts on (module docs: the
    /// safety net), the oldest look first, in rounds of
    /// [`AutoMergeLimits::batch`](super::AutoMergeLimits::batch) and at
    /// most [`SWEEP_ROUNDS_MAX`] rounds. Each merge request is decided
    /// once: a round lists what was not looked at since the sweep began.
    pub async fn sweep(&self) -> Result<AutoMergeReport, ForgeError> {
        let db = &self.merger.db;
        db::forget_inactive(db, 200).await?;
        let began = Instant::now();
        let mut report = AutoMergeReport::default();
        let mut looked: HashSet<i64> = HashSet::new();
        for _ in 0..SWEEP_ROUNDS_MAX {
            let due = db::candidates(db, self.limits.batch, began.elapsed()).await?;
            let more = i64::try_from(due.len()).unwrap_or(i64::MAX) >= self.limits.batch;
            // A merge request whose look was not recorded (it failed, it
            // became inactive) is listed again: once per sweep.
            let fresh: Vec<Candidate> = due
                .into_iter()
                .filter(|c| looked.insert(c.merge_request.get()))
                .collect();
            if fresh.is_empty() {
                break;
            }
            report.add(&self.decide_at_once(fresh).await);
            if !more {
                break;
            }
        }
        Ok(report)
    }
}

/// Why [`AutoMergeWatch::wait`] ended: what [`AutoMergeWatch::act`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoMergeWake {
    /// Every merge request (the safety net; the listener reconnected).
    Sweep,
    /// Events were sequenced: the merge requests they name.
    Events,
    /// These merge requests can be merged now.
    Named(Vec<i64>),
    /// No live listener: the poll of before plan 18.
    Poll,
}

impl AutoMergeWake {
    /// The word of the worker's log line.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sweep => "sweep",
            Self::Events => "events",
            Self::Named(_) => "named",
            Self::Poll => "poll",
        }
    }
}

#[derive(Debug)]
struct Listening {
    events: WakeRx,
    named: WakeRx,
}

/// The auto-merge worker with its wait (module docs). Not shared: one
/// task owns it.
#[derive(Debug)]
pub struct AutoMergeWatch {
    worker: AutoMerger,
    /// `auto_merge.safety_poll_ms`: the sweep's period.
    safety: Duration,
    /// `None` without a hub.
    rx: Option<Listening>,
    /// The sequence number of the last event read.
    cursor: i64,
    next_sweep: Instant,
    /// The last wait was the fallback poll: sweep when the listener is up.
    polled: bool,
}

impl AutoMerger {
    /// This worker with its wait: it sweeps at once, then every `safety`
    /// (`auto_merge.safety_poll_ms`), and between sweeps decides what it
    /// is told about on the wake hub of its database (module docs).
    pub fn watch(&self, safety: Duration) -> AutoMergeWatch {
        let rx = self.merger.db.wake().map(|hub| Listening {
            events: hub.subscribe(Channel::Events),
            named: hub.subscribe(Channel::AutoMerge),
        });
        AutoMergeWatch {
            worker: self.clone(), // clone: handles
            safety,
            rx,
            cursor: 0,
            next_sweep: Instant::now(),
            polled: false,
        }
    }
}

impl AutoMergeWatch {
    /// The poll while no hub listens for this process.
    pub const FALLBACK_POLL: Duration = FALLBACK_POLL;

    /// Whether this worker waits on a wake hub (false: it polls).
    pub fn waits_on_wake(&self) -> bool {
        self.rx.is_some()
    }

    /// The sweep's period.
    pub fn safety(&self) -> Duration {
        self.safety
    }

    fn live(&self) -> bool {
        self.rx.is_some()
            && self
                .worker
                .merger
                .db
                .wake()
                .is_some_and(|hub| hub.is_live(Channel::Events) && hub.is_live(Channel::AutoMerge))
    }

    /// Wait for the next thing to do (module docs). Cancel-safe: a dropped
    /// wait loses no wake.
    pub async fn wait(&mut self) -> AutoMergeWake {
        let live = self.live();
        let (Some(rx), true) = (self.rx.as_mut(), live) else {
            self.polled = true;
            tokio::time::sleep(FALLBACK_POLL.min(self.safety)).await;
            return AutoMergeWake::Poll;
        };
        if self.polled {
            // The listener is back: what was sent meanwhile is lost.
            self.polled = false;
            return AutoMergeWake::Sweep;
        }
        let left = self.next_sweep.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return AutoMergeWake::Sweep;
        }
        tokio::select! {
            woken = rx.events.wait(left) => match woken {
                Woken::Notified(_) => AutoMergeWake::Events,
                Woken::SafetyNet | Woken::Reconnected => AutoMergeWake::Sweep,
            },
            woken = rx.named.wait(left) => match woken {
                Woken::Notified(payloads) => {
                    let mut ids: Vec<i64> =
                        payloads.iter().filter_map(|p| p.parse().ok()).collect();
                    ids.sort_unstable();
                    ids.dedup();
                    AutoMergeWake::Named(ids)
                }
                Woken::SafetyNet | Woken::Reconnected => AutoMergeWake::Sweep,
            },
        }
    }

    /// Do what `wake` asks for (module docs). A failure to list or read is
    /// returned (the caller backs off); a failed sweep is tried again
    /// after [`AutoMergeWatch::FALLBACK_POLL`] at the earliest.
    pub async fn act(&mut self, wake: AutoMergeWake) -> Result<AutoMergeReport, ForgeError> {
        match wake {
            AutoMergeWake::Sweep => {
                self.next_sweep = Instant::now() + FALLBACK_POLL.min(self.safety);
                // Read before the sweep: an event sequenced during it is
                // read after it (deciding twice is harmless).
                let high = db::events_high(&self.worker.merger.db).await?;
                let report = self.worker.sweep().await?;
                self.cursor = high;
                self.next_sweep = Instant::now() + self.safety;
                Ok(report)
            }
            AutoMergeWake::Events => self.worker.decide_events(&mut self.cursor).await,
            AutoMergeWake::Named(ids) => self.worker.decide_named(&ids).await,
            AutoMergeWake::Poll => self.worker.tick().await,
        }
    }
}
