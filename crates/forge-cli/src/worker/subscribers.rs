//! The `subscribers` task (ADR-0016 §7): one forge-db
//! [`Dispatcher`](forge_db::subscribers::Dispatcher) for every registered
//! subscriber. A run is one round — each subscriber that is behind
//! `high_seq` handles up to a batch of events, one per transaction — then,
//! unless a batch was full, a wait for the next NOTIFY (or the 5 s poll) on
//! the dedicated LISTEN connection. Either way the next run starts at once:
//! the wait is the pacing. A round starts no new step after 30 s
//! (`ROUND_BUDGET`, then it asks for more), so a shutdown request is
//! noticed within one poll (≤ 5 s) or one round (≤ 30 s plus the step in
//! progress, itself ≤ 40 s).
//!
//! This task is the logging boundary of a round: each incident (a failed
//! handler, a stopped or quarantining subscriber — `FG-DB-0008`) is logged
//! once with the source event's request id. A subscriber that keeps failing
//! is retried with its own backoff inside the dispatcher (≤ 1 per minute),
//! so its log lines stay bounded too. The task itself fails (and backs
//! off) only when the round cannot run at all. A failed wait (the LISTEN
//! connection: `FG-DB-0001` / `FG-DB-0002`) is logged once here, then the
//! run sleeps one poll interval and asks for the next round — the 5 s poll
//! goes on without notifications, and the next wait reconnects.
//!
//! Cancel-safe: the dispatcher's steps are single transactions, and the
//! state kept between runs (cursor backoff, the listener) is valid at every
//! `.await`.

use std::sync::Arc;
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_db::subscribers::{Dispatcher, Registry, POLL};
use forge_db::Db;
use forge_errors::ForgeError;
use tokio::sync::Mutex;

use super::supervise::{Next, Runner};

pub(super) const NAME: &str = "subscribers";
/// The pause after [`super::backoff::MAX_IMMEDIATE`] immediate runs.
pub(super) const PERIOD: Duration = Duration::from_millis(200);

/// The dispatcher is created by the first run (registration needs the
/// database, and a failure there is retried like any other).
pub(super) fn runner(db: &Db, registry: &Registry) -> Runner {
    let state: Arc<Mutex<Option<Dispatcher>>> = Arc::new(Mutex::new(None));
    let db = db.clone(); // clone: pool handles (Arc inside)
    let registry = registry.clone(); // clone: Arcs of the subscribers
    Runner {
        name: NAME,
        period: PERIOD,
        run: Box::new(move || {
            let db = db.clone(); // clone: each run future owns a pool handle
            let registry = registry.clone(); // clone: Arcs of the subscribers
            let state = Arc::clone(&state);
            Box::pin(async move { run(&db, &registry, &state).await })
        }),
    }
}

async fn run(
    db: &Db,
    registry: &Registry,
    state: &Mutex<Option<Dispatcher>>,
) -> Result<Next, ForgeError> {
    // Never contended: the supervisor runs one run of a task at a time.
    let mut guard = state.lock().await;
    let dispatcher = match guard.as_mut() {
        Some(d) => d,
        None => guard.insert(Dispatcher::start(db, registry).await?),
    };
    let round = dispatcher.round(db).await?;
    for incident in round.incidents {
        let request_id = incident.request_id.unwrap_or_else(RequestId::generate);
        let mut error = incident.error.with_context("task", NAME);
        if let Some(seq) = incident.seq {
            error = error.with_context("source_seq", seq);
        }
        if let Some(event_id) = incident.event_id {
            error = error.with_context("source_event_id", event_id);
        }
        telemetry::log_failure(&error, &request_id);
    }
    if round.handled > 0 {
        tracing::debug!(
            task = NAME,
            handled = round.handled,
            "forge.worker.subscribers_round"
        );
    }
    if round.more {
        return Ok(Next::Immediately);
    }
    if let Err(e) = dispatcher.wait(db).await {
        telemetry::log_failure(&e.with_context("task", NAME), &RequestId::generate());
        tokio::time::sleep(POLL).await;
    }
    Ok(Next::Immediately)
}

#[cfg(test)]
#[path = "subscribers_tests.rs"]
mod tests;
