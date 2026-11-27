//! The gateway's background task (one per worker): every second the clock
//! ([`Gateway::tick`]); every `llm.kanflow_poll_ms` Kanflow's `ai-busy`;
//! every [`GPU_EVAL_POLL`] whether a `gpu-eval` job runs; the model digest
//! at start and every [`DIGEST_POLL`]; the instance's stored pause every
//! [`PAUSE_POLL`] (an administrator's `POST /api/v1/admin/llm/pause`); a
//! metrics snapshot (`forge.llm.metrics`, with the feedback totals read
//! from the database) every [`METRICS_LOG`].
//!
//! Failures are logged once per streak (with a fresh `request_id` each):
//! `FG-LLM-0003` (alert) when the digest is not the pinned one,
//! `FG-LLM-0001` when Ollama cannot be asked, the database's code when the
//! `gpu-eval` read fails (the last answer stays). A recovery is logged at
//! info. Kanflow's state is logged when it changes.

use std::sync::Arc;
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_db::Db;
use forge_errors::ErrorCode;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::kanflow::BusySource;
use crate::sched::Kanflow;
use crate::{Gateway, ModelCatalog};

pub const GPU_EVAL_POLL: Duration = Duration::from_secs(10);
pub const DIGEST_POLL: Duration = Duration::from_secs(10 * 60);
pub const METRICS_LOG: Duration = Duration::from_secs(5 * 60);
/// How often the instance's stored pause (`forge.llm_gateway`) is read.
pub const PAUSE_POLL: Duration = Duration::from_secs(10);
/// Reads of the stored pause that fail in a row before the gateway pauses
/// itself (30 s): an administrator's pause is never ignored for long.
pub const PAUSE_FAILURES_BEFORE_PAUSE: u32 = 3;

/// What the task watches.
pub struct Background {
    pub busy: Arc<dyn BusySource>,
    pub catalog: Arc<dyn ModelCatalog>,
    pub kanflow_poll: Duration,
}

/// A failure streak: log its first failure and its recovery only.
#[derive(Debug, Default)]
struct Streak {
    code: Option<ErrorCode>,
}

impl Streak {
    /// `true` when this failure starts a streak (or changes its code).
    fn fail(&mut self, code: ErrorCode) -> bool {
        let new = self.code != Some(code);
        self.code = Some(code);
        new
    }

    /// `true` when this success ends a streak.
    fn ok(&mut self) -> bool {
        self.code.take().is_some()
    }
}

/// Run the task until a stop is requested.
pub fn spawn(
    gw: Gateway,
    db: Db,
    bg: Background,
    mut stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let start = Instant::now();
        let mut next_kanflow = start;
        let mut next_gpu = start;
        let mut next_digest = start;
        let mut next_metrics = start + METRICS_LOG;
        let mut kanflow = Kanflow::Idle;
        let mut digest = Streak::default();
        let mut gpu = Streak::default();
        let mut next_pause = start;
        let mut pause = Streak::default();
        let mut pause_failures: u32 = 0;
        loop {
            if *stop.borrow() {
                return;
            }
            let now = Instant::now();
            if now >= next_kanflow {
                next_kanflow = now + bg.kanflow_poll;
                let state = bg.busy.poll().await;
                if state != kanflow {
                    tracing::info!(
                        target: "forge_llm",
                        event = "forge.llm.kanflow_gate",
                        state = ?state,
                        "Kanflow's GPU gate changed"
                    );
                    kanflow = state;
                    gw.set_conditions(Some(state), None);
                }
            }
            if now >= next_gpu {
                next_gpu = now + GPU_EVAL_POLL;
                match forge_db::llm::gpu_eval_running(&db).await {
                    Ok(running) => {
                        let _recovered = gpu.ok();
                        gw.set_conditions(None, Some(running));
                    }
                    Err(e) => {
                        if gpu.fail(e.code()) {
                            telemetry::log_failure(
                                &e.with_context("task", "llm_gpu_eval_poll"),
                                &RequestId::generate(),
                            );
                        }
                    }
                }
            }
            if now >= next_pause {
                next_pause = now + PAUSE_POLL;
                match forge_db::llm::ops::gateway_pause(&db).await {
                    Ok(stored) => {
                        let _recovered = pause.ok();
                        pause_failures = 0;
                        if gw.follow_stored_pause(stored.paused) {
                            tracing::warn!(
                                target: "forge_llm",
                                event = "forge.llm.pause_followed",
                                paused = gw.is_paused(),
                                stored_paused = stored.paused,
                                config_paused = gw.config().paused,
                                changed_by = stored.changed_by.map(forge_core::UserId::get),
                                changed_at = stored.changed_at.as_deref(),
                                "the LLM gateway follows the instance's stored pause"
                            );
                        }
                    }
                    Err(e) => {
                        pause_failures += 1;
                        if pause.fail(e.code()) {
                            telemetry::log_failure(
                                &e.with_context("task", "llm_pause_poll"),
                                &RequestId::generate(),
                            );
                        }
                        // Fail closed: an administrator's pause that cannot
                        // be read is not ignored for long. The next good
                        // read follows the stored state again.
                        if pause_failures == PAUSE_FAILURES_BEFORE_PAUSE && !gw.is_paused() {
                            gw.pause();
                            tracing::warn!(
                                target: "forge_llm",
                                event = "forge.llm.pause_followed",
                                paused = true,
                                reason = "pause_unreadable",
                                failures = pause_failures,
                                "the instance's pause could not be read; the LLM gateway pauses until it can"
                            );
                        }
                    }
                }
            }
            if now >= next_digest {
                next_digest = now + DIGEST_POLL;
                match gw.verify_model(&*bg.catalog).await {
                    Ok(()) => {
                        if digest.ok() {
                            tracing::info!(
                                target: "forge_llm",
                                event = "forge.llm.model_verified",
                                model = %gw.config().model,
                                "the LLM model digest matches again; inference resumed"
                            );
                        }
                    }
                    Err(e) => {
                        if digest.fail(e.code()) {
                            telemetry::log_failure(
                                &e.with_context("task", "llm_model_check"),
                                &RequestId::generate(),
                            );
                        }
                    }
                }
            }
            if now >= next_metrics {
                next_metrics = now + METRICS_LOG;
                // `forge_llm_advice_feedback_total{purpose,verdict}`: counted
                // from the table (its rows are never deleted).
                match forge_db::llm::ops::feedback_totals(&db).await {
                    Ok(totals) => gw.metrics().set_feedback_totals(&totals),
                    Err(e) => {
                        // Never a stale total shown as current.
                        gw.metrics().set_feedback_totals(&[]);
                        telemetry::log_failure(
                            &e.with_context("task", "llm_feedback_totals"),
                            &RequestId::generate(),
                        );
                    }
                }
                let (waiting, running) = gw.queue_len();
                tracing::info!(
                    target: "forge_llm",
                    event = "forge.llm.metrics",
                    waiting,
                    running,
                    paused = gw.is_paused(),
                    stopped = gw.is_stopped(),
                    metrics = %gw.metrics().render(),
                    "LLM gateway metrics"
                );
            }
            gw.tick();
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
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
mod tests {
    use super::*;

    #[test]
    fn a_streak_logs_its_start_and_its_end_once() {
        let mut s = Streak::default();
        assert!(s.fail(ErrorCode::LlmUnavailable));
        assert!(!s.fail(ErrorCode::LlmUnavailable));
        assert!(s.fail(ErrorCode::LlmModelDigestMismatch));
        assert!(s.ok());
        assert!(!s.ok());
    }
}
