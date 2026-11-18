//! What an idle [`JobRunner`](super::JobRunner) waits for (plan 18 S2,
//! ADR-0037 (a)): a `forge_jobs` notification for one of its kinds, the
//! time the next scheduled job is due, or its safety-net period
//! (`jobs.safety_poll_ms`) — whichever comes first. The wake is a hint:
//! the caller claims again and the claim reads the queue.
//!
//! - **Bursts.** All runs of a runner share ONE subscription, and one
//!   notification wakes ONE idle run. A run whose claim succeeds pokes the
//!   next idle run (several jobs may stand behind one notification), so
//!   `n` runs start within `n` claims and a burst of any size costs at
//!   most one claim that finds nothing per idle run — not one per
//!   notification (a poke no idle run took is taken back when the next
//!   claim starts: that claim is the look it asked for). Runs that are
//!   busy are not woken at all: they claim again when their job ends, as
//!   before. A notification for a kind that
//!   is at its cap when it arrives wakes no one either (the run of that
//!   kind that ends next claims again by itself).
//! - **Scheduled jobs** (`run_after` in the future: a retry after its
//!   backoff, a delayed enqueue) send no notification when they become
//!   ready. Every wait therefore reads the next due time of the runner's
//!   kinds first and ends [`DUE_MARGIN`] after it. The statement that
//!   schedules a job (a delayed enqueue, the backoff of a failure or of an
//!   expired lease) notifies, so a waiter that read the due time earlier
//!   reads it again.
//! - **Listener down** ([`WakeHub::is_live`] false): the wait is at most
//!   [`FALLBACK_POLL`] — the poll of before plan 18.
//! - **`FG-DB-0024 WakeupMissed`**: a wait that ran its whole safety-net
//!   period while the listener was up, followed by a claim of a job that
//!   had been ready for at least that period, while no run of this runner
//!   ended and no wake arrived in that time (a busy ordering key or a kind
//!   at its cap frees with the end of a run). The hub logs it at most once
//!   a minute.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use tokio::sync::Notify;
use tokio::time::Instant;

use super::{ClaimedJob, JobKind, WorkerId};
use crate::wake::{Channel, WakeHub, WakeRx, Woken};
use crate::{query_error, Db};

/// The longest wait while the LISTEN connection is down.
pub const FALLBACK_POLL: Duration = Duration::from_secs(1);
/// A wait for a scheduled job ends this long after its due time (the due
/// time is the database's clock, the wait is this process's).
pub const DUE_MARGIN: Duration = Duration::from_millis(20);
/// No wait is shorter (a due time that has just passed).
const MIN_WAIT: Duration = Duration::from_millis(10);

/// The idle wait of one runner, shared by its runs.
#[derive(Debug)]
pub(super) struct JobWake {
    hub: WakeHub,
    safety: Duration,
    /// The one subscription: the run that holds it waits for the hub.
    rx: tokio::sync::Mutex<WakeRx>,
    /// A claim succeeded: one more idle run should look.
    poke: Notify,
    /// The last wait ran the whole safety-net period with the hub live.
    after_safety_net: AtomicBool,
    /// The last wake, or the last end of a run.
    quiet_since: Mutex<Instant>,
}

impl JobWake {
    /// The wait of a runner on `db`; `None` without a hub attached
    /// ([`Db::with_wake`]): the caller keeps its own poll.
    pub(super) fn of(db: &Db) -> Option<Self> {
        let (hub, safety) = (db.wake()?, db.jobs_safety_poll()?);
        Some(Self {
            hub: hub.clone(), // clone: a handle on the shared hub
            safety,
            rx: tokio::sync::Mutex::new(hub.subscribe(Channel::Jobs)),
            poke: Notify::new(),
            after_safety_net: AtomicBool::new(false),
            quiet_since: Mutex::new(Instant::now()),
        })
    }

    fn stir(&self) {
        *self
            .quiet_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn quiet_for(&self) -> Duration {
        self.quiet_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }

    /// Wait until a claim for `kinds` is worth another try (module docs);
    /// `takes_now` says whether a claim could take a job of a kind at the
    /// moment its notification arrives. `Err`: the due time could not be
    /// read (the caller's failure policy paces the retries).
    pub(super) async fn wait(
        &self,
        db: &Db,
        kinds: &[JobKind],
        takes_now: impl Fn(&str) -> bool,
    ) -> Result<(), ForgeError> {
        let live = self.hub.is_live(Channel::Jobs);
        let period = if live {
            self.safety
        } else {
            self.safety.min(FALLBACK_POLL)
        };
        let wait = match next_due(db, kinds).await? {
            Some(due) => period.min(due.saturating_add(DUE_MARGIN)),
            None => period,
        }
        .max(MIN_WAIT);
        let woken = tokio::select! {
            () = self.poke.notified() => None,
            woken = async {
                let mut rx = self.rx.lock().await;
                rx.wait_matching(wait, takes_now).await
            } => Some(woken),
            () = tokio::time::sleep(wait) => Some(Woken::SafetyNet),
        };
        let whole_safety_net = matches!(woken, Some(Woken::SafetyNet))
            && live
            && wait >= self.safety
            && self.hub.is_live(Channel::Jobs);
        if !whole_safety_net {
            self.stir();
        }
        self.after_safety_net
            .store(whole_safety_net, Ordering::Relaxed);
        Ok(())
    }

    /// A claim starts (called under the runner's claim gate, before it
    /// reads what is claimable): a poke no idle run took asked for a look,
    /// and this claim is that look — it is taken back, so that it does not
    /// end the wait that follows a claim that finds nothing. A poke that
    /// arrives from here on stays.
    pub(super) fn claim_starts(&self) {
        let stale = std::pin::pin!(self.poke.notified());
        // `enable` takes a stored permit; without one the registration
        // ends with this scope.
        let _taken = stale.enable();
    }

    /// A claim found nothing.
    pub(super) fn found_nothing(&self) {
        self.after_safety_net.store(false, Ordering::Relaxed);
    }

    /// A claim took `job`, ready for `ready_for`: the next idle run looks
    /// too, and a find no wake announced is reported (`FG-DB-0024`).
    pub(super) fn claimed(&self, job: &ClaimedJob, ready_for: Duration, worker: &WorkerId) {
        let after_safety_net = self.after_safety_net.swap(false, Ordering::Relaxed);
        self.poke.notify_one();
        let missed = after_safety_net
            && ready_for >= self.safety
            && self.quiet_for() >= self.safety
            && self.hub.is_live(Channel::Jobs);
        if missed {
            let error = ForgeError::new(ErrorCode::WakeupMissed)
                .with_context("oldest_ready_ms", ready_for.as_millis())
                .with_context("safety_poll_ms", self.safety.as_millis())
                .with_context("kind", job.kind.as_str())
                .with_context("job_id", job.lease.job_id.get())
                .with_context("worker_id", worker.as_str());
            self.hub.missed(Channel::Jobs, error, &job.request_id());
        }
    }

    /// A run ended: what it kept others from claiming (its ordering key,
    /// its kind's place) is free again.
    pub(super) fn run_ended(&self) {
        self.stir();
    }

    /// The place of a run left leased is free again (its lease ended): no
    /// run of its own claims after it, so an idle one is told.
    pub(super) fn place_freed(&self) {
        self.stir();
        self.poke.notify_one();
    }
}

/// How long until the next scheduled job of `kinds` is due (`None`: no
/// such job).
async fn next_due(db: &Db, kinds: &[JobKind]) -> Result<Option<Duration>, ForgeError> {
    if kinds.is_empty() {
        return Ok(None);
    }
    let kinds: Vec<&str> = kinds.iter().map(JobKind::as_str).collect();
    let due_ms: Option<i64> = db
        .within("jobs.next_due", async {
            sqlx::query_scalar(
                "SELECT (EXTRACT(epoch FROM j.run_after - pg_catalog.now()) * 1000)::bigint \
                   FROM ONLY forge.jobs AS j \
                  WHERE j.state = 'queued' AND j.run_after > pg_catalog.now() \
                    AND j.attempts < j.max_attempts AND j.kind = ANY($1::text[]) \
                  ORDER BY j.run_after LIMIT 1",
            )
            .bind(&kinds)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error("jobs.next_due", e))
        })
        .await?;
    // Due in the past by the time it is read: as soon as possible.
    Ok(due_ms.map(|ms| Duration::from_millis(u64::try_from(ms).unwrap_or(0))))
}
