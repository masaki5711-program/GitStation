//! The worker's local LLM tasks (plan 19 L5-3, ADR-0039).
//!
//! - Always: the records' upkeep every [`UPKEEP_PERIOD`] —
//!   `forge_db::llm::abandon_stale` (live rows a dead worker left,
//!   `FG-LLM-0001`, logged as `forge.llm.abandoned_stale`) and
//!   `sweep_retention` (requests 90 days, advice 180 days; feedback stays).
//!   A failure is logged once per streak with its code.
//! - With `llm.enabled`: the [`Gateway`] (the Ollama provider with
//!   `llm.ca_file`'s roots) and its background task (Kanflow's `ai-busy`,
//!   the `gpu-eval` state, the model digest, metrics). The CA and the
//!   Kanflow token are read now: a bad one stops the worker's start
//!   (`FG-SYS-0002`).
//!
//!   With the gateway, the approval summaries of deployments waiting for
//!   an approval ([`super::approval_summary`], plan 19 L5-8).
//!
//! Disabled (the default, and production until a GPU host is named): no
//! gateway, no call to any model; the features that would ask fall back
//! to their view without AI.

use std::sync::Arc;
use std::time::Duration;

use forge_core::config::LlmConfig;
use forge_core::deploy::TargetPolicy;
use forge_core::{telemetry, RequestId};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_llm::background::{self, Background};
use forge_llm::kanflow::{BusySource, KanflowGate, NoGate};
use forge_llm::ollama::OllamaProvider;
use forge_llm::{DbRecords, Gateway, GatewayConfig};
use tokio::sync::watch;

use super::provision::Storage;
use tokio::task::JoinHandle;

/// How often the records' upkeep runs.
pub(crate) const UPKEEP_PERIOD: Duration = Duration::from_secs(60 * 60);
/// Kanflow's `ai-busy` answers at once: a short timeout.
const KANFLOW_TIMEOUT: Duration = Duration::from_secs(2);

/// The running LLM tasks. The gateway lives in its background task; the
/// features of L5-5+ will take a handle from here.
pub(crate) struct LlmTasks {
    handles: Vec<JoinHandle<()>>,
}

impl LlmTasks {
    pub(crate) fn abort(self) {
        for h in self.handles {
            h.abort();
        }
    }
}

/// Start the tasks (module docs).
pub(crate) fn start(
    cfg: &LlmConfig,
    db: &Db,
    storage: &Storage,
    stop: &watch::Receiver<bool>,
) -> Result<LlmTasks, ForgeError> {
    let mut handles = vec![spawn_upkeep(db.clone(), stop.clone())]; // clone: the task's own handles
    match (cfg.enabled, cfg.endpoint.as_deref(), cfg.model.as_ref()) {
        (true, Some(endpoint), Some(model)) => {
            let ca = cfg.ca_pem()?;
            let provider = Arc::new(OllamaProvider::new(endpoint, ca.as_deref(), cfg.timeout)?);
            let busy: Arc<dyn BusySource> =
                match (cfg.kanflow_busy_url.as_deref(), cfg.kanflow_token()?) {
                    (Some(url), Some(token)) => Arc::new(KanflowGate::new(
                        url,
                        token,
                        ca.as_deref(),
                        KANFLOW_TIMEOUT,
                    )?),
                    _ => Arc::new(NoGate),
                };
            let gateway = Gateway::new(
                GatewayConfig {
                    model: model.clone(), // clone: the gateway owns its settings
                    num_ctx: cfg.num_ctx,
                    concurrency: cfg.concurrency,
                    paused: cfg.paused,
                    call_timeout: cfg.timeout,
                },
                provider.clone(), // clone: the provider is also the model catalog
                Arc::new(DbRecords(db.clone())), // clone: the records' own pool handle
            );
            // Plan 19 L5-8: the approval summaries (the facts read with git).
            let browse = Browser::new(
                storage.git.clone(),  // clone: the browser owns its handles
                storage.root.clone(), // clone: as git
                BrowseLimits::default(),
            );
            let deploy = forge_deploy::Deploy::new(
                db.clone(), // clone: the service's own pool handle
                Arc::new(forge_deploy::GitCommits::new(db.clone(), browse)), // clone: as above
                TargetPolicy::default(), // the facts never reach a target
            );
            handles.push(super::approval_summary::spawn(
                gateway.clone(), // clone: the task's own handle
                db.clone(),      // clone: the task's own pool handle
                deploy,
                stop.clone(), // clone: the task waits on its own receiver
            ));
            handles.push(background::spawn(
                gateway.clone(), // clone: the task's own handle
                db.clone(),      // clone: the task's own pool handle
                Background {
                    busy,
                    catalog: provider,
                    kanflow_poll: cfg.kanflow_poll,
                },
                stop.clone(), // clone: the task waits on its own receiver
            ));
            tracing::info!(
                target: "forge_cli",
                event = "forge.llm.gateway_started",
                endpoint,
                model = %model,
                concurrency = cfg.concurrency,
                paused = cfg.paused,
                kanflow_gate = cfg.kanflow_busy_url.is_some(),
                "the local LLM gateway is running"
            );
        }
        _ => {
            tracing::info!(
                target: "forge_cli",
                event = "forge.llm.gateway_disabled",
                "llm.enabled is false: no LLM advice is made"
            );
        }
    }
    Ok(LlmTasks { handles })
}

/// One upkeep pass; `Err` with the failing step's error.
pub(crate) async fn upkeep(db: &Db, request_id: &RequestId) -> Result<(), ForgeError> {
    let abandoned = forge_db::llm::abandon_stale(
        db,
        ErrorCode::LlmUnavailable,
        forge_db::llm::STALE_LIVE_SECS,
    )
    .await?;
    if abandoned > 0 {
        tracing::info!(
            target: "forge_cli",
            event = "forge.llm.abandoned_stale",
            request_id = %request_id,
            abandoned,
            "live LLM requests a stopped worker left were abandoned"
        );
    }
    let swept = forge_db::llm::sweep_retention(db).await?;
    if swept.advice > 0 || swept.requests > 0 {
        tracing::info!(
            target: "forge_cli",
            event = "forge.llm.retention_swept",
            request_id = %request_id,
            advice = swept.advice,
            requests = swept.requests,
            "old LLM records were deleted (feedback is kept)"
        );
    }
    Ok(())
}

fn spawn_upkeep(db: Db, mut stop: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut failing: Option<ErrorCode> = None;
        loop {
            if *stop.borrow() {
                return;
            }
            let request_id = RequestId::generate();
            match upkeep(&db, &request_id).await {
                Ok(()) => failing = None,
                Err(e) => {
                    if failing != Some(e.code()) {
                        failing = Some(e.code());
                        telemetry::log_failure(&e.with_context("task", "llm_upkeep"), &request_id);
                    }
                }
            }
            tokio::select! {
                () = tokio::time::sleep(UPKEEP_PERIOD) => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return;
                    }
                }
            }
        }
    })
}
