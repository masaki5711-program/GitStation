//! The wake hub of a long-running process (plan 18 S2, ADR-0037 (a);
//! `forge_db::wake`): `forge serve` and `forge worker` start ONE hub each
//! — one LISTEN connection per process, on top of its two pools — and
//! attach it to their [`Db`], so every job runner and runner long poll of
//! the process waits on it instead of polling. One-shot commands start
//! none and behave as before plan 18.

use std::time::Duration;

use forge_core::config::Config;
use forge_core::{telemetry, RequestId};
use forge_db::wake::{Channel, WakeHub};
use forge_db::Db;
use forge_errors::ForgeError;

/// How long a shutdown waits for the hub's task (it holds no work: it
/// ends as soon as it sees the stop).
const STOP_WAIT: Duration = Duration::from_secs(5);

/// Start the hub of this process and attach it to `db` (before the handle
/// is cloned: only clones made afterwards carry the hub). The hub connects
/// in the background; a database that refuses the LISTEN connection is
/// logged (`FG-DB-0001` / `FG-DB-0002` with `operation=wake_listen`) and
/// the runners poll every second meanwhile.
pub(crate) fn attach(db: Db, cfg: &Config) -> Result<(Db, WakeHub), ForgeError> {
    let hub = WakeHub::spawn(&db, &Channel::ALL)?;
    // clone: the handle the caller stops at shutdown
    let db = db.with_wake(hub.clone(), cfg.jobs.safety_poll());
    Ok((db, hub))
}

/// End the hub's task and wait for it, before the pools are closed. A
/// task that panicked or did not end in time is logged, never ignored.
pub(crate) async fn stop(hub: &WakeHub) {
    if let Err(e) = hub.stop(STOP_WAIT).await {
        telemetry::log_failure(&e.with_context("task", "wake_hub"), &RequestId::generate());
    }
}
