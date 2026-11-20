//! The worker task `org_master_sync` (ADR-0038 §1): every
//! `org.master_sync_interval_secs` (default 15 min) the PFS org master is
//! copied ([`forge_integ::org_master::OrgMasterSync::run_once`]). Present
//! only with `org.master_sync_enabled`.
//!
//! - A failed run keeps the previous copy; the next one waits
//!   [`RETRY_BASE`] × 2^n after the n-th failure in a row (1 min, 2 min,
//!   4 min, …), at most the interval ([`Backoff`]). The failure is logged when
//!   a streak starts or changes and again at most every minute
//!   (`FG-INT-0025` / `0026` / `0027`, or the database's code), with the
//!   run's request id; the first success after a streak is logged
//!   (`forge.org.master_sync_recovered`).
//! - When the copy is older than `org.master_max_age_hours` (or was never
//!   written) after a failed run, `FG-INT-0028` is logged once until the
//!   next success: everything derived from the copy now fails closed.
//! - A sync that changed the copy is logged (`forge.org.master_synced`,
//!   counts only). Another worker syncing at the same moment (the advisory
//!   lock is held) is skipped with an info log
//!   (`forge.org.master_sync_busy`). Tokens and names are never logged.

use std::time::Duration;

use forge_core::config::OrgConfig;
use forge_core::{telemetry, RequestId};
use forge_db::org_master::Replaced;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_integ::org_master::OrgMasterSync;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::backoff::{Backoff, Next, Signature};

/// The backoff's base (30 s): the n-th failure in a row waits
/// `RETRY_BASE × 2^n` — 1 min after the first, 2 min after the second —
/// at most the interval.
pub(crate) const RETRY_BASE: Duration = Duration::from_secs(30);

/// The task's state between runs.
#[derive(Debug)]
pub(crate) struct Lane {
    backoff: Backoff,
    interval: Duration,
    max_age: Duration,
    /// `FG-INT-0028` was logged for the current streak.
    stale_logged: bool,
    /// The staleness check itself failed and was logged in this streak.
    health_logged: bool,
}

/// What one [`Lane::step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stepped {
    /// Wait this long before the next run.
    pub(crate) delay: Duration,
    pub(crate) ok: bool,
    /// The failure was logged.
    pub(crate) logged: bool,
    /// `FG-INT-0028` was logged by this step.
    pub(crate) stale: bool,
}

impl Lane {
    pub(crate) fn new(cfg: &OrgConfig) -> Self {
        Self {
            backoff: Backoff::new(RETRY_BASE, cfg.master_sync_interval.max(RETRY_BASE)),
            interval: cfg.master_sync_interval,
            max_age: cfg.master_max_age,
            stale_logged: false,
            health_logged: false,
        }
    }

    /// One run and its logging.
    pub(crate) async fn step(&mut self, db: &Db, sync: &OrgMasterSync) -> Stepped {
        let request_id = RequestId::generate();
        let e = match sync.run_once(&request_id).await {
            Ok(Replaced::Busy) => {
                tracing::info!(
                    target: "forge_cli",
                    event = "forge.org.master_sync_busy",
                    request_id = %request_id,
                    "another worker is syncing the org master; this run was skipped"
                );
                return Stepped {
                    delay: self.interval,
                    ok: true,
                    logged: false,
                    stale: false,
                };
            }
            Ok(Replaced::Synced(done)) => {
                let decision = self.backoff.success(Next::AfterPeriod);
                self.stale_logged = false;
                self.health_logged = false;
                if let Some(failures) = decision.recovered_after {
                    tracing::info!(
                        target: "forge_cli",
                        event = "forge.org.master_sync_recovered",
                        request_id = %request_id,
                        failures,
                        "the org master copy was synced again"
                    );
                }
                if done.units.touched() || done.positions.touched() {
                    tracing::info!(
                        target: "forge_cli",
                        event = "forge.org.master_synced",
                        request_id = %request_id,
                        units_added = done.units.added,
                        units_changed = done.units.changed,
                        units_removed = done.units.removed,
                        units = done.units.total,
                        positions_added = done.positions.added,
                        positions_changed = done.positions.changed,
                        positions_removed = done.positions.removed,
                        positions = done.positions.total,
                        "the org master copy changed"
                    );
                }
                return Stepped {
                    delay: self.interval,
                    ok: true,
                    logged: false,
                    stale: false,
                };
            }
            Err(e) => e,
        };
        let decision = self.backoff.failure(Signature::of(&e), Instant::now());
        if decision.log_failure {
            telemetry::log_failure(
                &e.with_context("task", "org_master_sync")
                    .with_context("failures", decision.failures),
                &request_id,
            );
        }
        let stale = self.check_stale(db, &request_id).await;
        Stepped {
            delay: decision.delay,
            ok: false,
            logged: decision.log_failure,
            stale,
        }
    }

    /// `FG-INT-0028` once per streak when the copy is too old; `true` when
    /// logged now.
    async fn check_stale(&mut self, db: &Db, request_id: &RequestId) -> bool {
        if self.stale_logged {
            return false;
        }
        match forge_db::org_master::health(db).await {
            Ok(h) => {
                let max = i64::try_from(self.max_age.as_secs()).unwrap_or(i64::MAX);
                if h.age_secs.is_none_or(|age| age > max) {
                    self.stale_logged = true;
                    let mut e = ForgeError::new(ErrorCode::OrgMasterStale)
                        .with_context("event", "forge.org.master_stale")
                        .with_context("units", h.units)
                        .with_context("max_age_hours", self.max_age.as_secs() / 3600);
                    if let Some(age) = h.age_secs {
                        e = e.with_context("age_secs", age);
                    }
                    telemetry::log_failure(&e, request_id);
                    return true;
                }
                false
            }
            Err(e) => {
                if !self.health_logged {
                    self.health_logged = true;
                    telemetry::log_failure(
                        &e.with_context("task", "org_master_stale_check"),
                        request_id,
                    );
                }
                false
            }
        }
    }
}

/// Run the task until a stop is requested (the first run at once).
pub(crate) fn spawn(
    db: Db,
    sync: OrgMasterSync,
    cfg: &OrgConfig,
    mut stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let mut lane = Lane::new(cfg);
    tokio::spawn(async move {
        loop {
            if *stop.borrow() {
                return;
            }
            let stepped = lane.step(&db, &sync).await;
            tokio::select! {
                () = tokio::time::sleep(stepped.delay) => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
#[path = "org_master_tests.rs"]
mod tests;
