//! `forge doctor` for the local LLM gateway (plan 19 §8.4, ADR-0039;
//! runbooks llm-gateway, llm-model-update).
//!
//! Without a database (all skipped while `llm.enabled` is off — the
//! default, and production until a GPU host is named):
//! - `llm.endpoint`: Ollama answers `/api/tags` (5 s), else `FG-LLM-0001`
//!   with `reason`. That the endpoint is in-house was checked when the
//!   configuration was loaded (`FG-SYS-0002 reason=llm_endpoint_not_internal`).
//! - `llm.model`: the pinned model is there with the pinned digest, else
//!   `FG-LLM-0003` (alert; `reason=model_missing` without the model).
//! - `llm.num_ctx`: the model, when loaded, runs with `llm.num_ctx` (`/api/ps`
//!   `context_length`). Another value means another user of the shared
//!   Ollama (Kanflow) loads it differently and every switch reloads the
//!   model: `warn` (no FG code — nothing fails, it is slow).
//! - `llm.kanflow_gate`: Kanflow's `ai-busy` answers (busy / idle). Not
//!   answering is a `warn`: the gateway then starts at most one request a
//!   minute and never blocks.
//!
//! With the database (also skipped while `llm.enabled` is off: no gateway
//! is meant to work the queue, and the worker's upkeep abandons what a
//! former one left after 7 hours): `llm.queue_age` — no request `queued` past its tier's
//! deadline (plus [`QUEUE_GRACE`]): a running gateway gives such a request
//! up (`FG-LLM-0004`) itself, so one still queued means no gateway works
//! the queue (the worker is down or stuck). Requests that gave up in the
//! last hour are a `warn`.
//!
//! Never part of `/readyz`: the LLM only advises (plan 19 §8.4).

use std::time::Duration;

use forge_core::config::{Config, LlmConfig};
use forge_core::llm::ModelRef;
use forge_db::llm::QueueHealth;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_llm::kanflow::KanflowGate;
use forge_llm::ollama::{Loaded, OllamaProvider};
use forge_llm::sched::Kanflow;
use forge_llm::ModelCatalog;

use super::{fail, ok, skip, warn, Check};

pub(super) const ENDPOINT: &str = "llm.endpoint";
pub(super) const MODEL: &str = "llm.model";
pub(super) const NUM_CTX: &str = "llm.num_ctx";
pub(super) const KANFLOW_GATE: &str = "llm.kanflow_gate";
/// The checks without a database, in their order.
pub(super) const NAMES: [&str; 4] = [ENDPOINT, MODEL, NUM_CTX, KANFLOW_GATE];
/// The database check.
pub(super) const QUEUE_AGE: &str = "llm.queue_age";

/// Every HTTP call of these checks.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How much longer than its deadline a queued request may still be listed:
/// the gateway's clock and the database's differ a little, and the row is
/// written after the request was given up.
const QUEUE_GRACE: Duration = Duration::from_secs(120);

const DISABLED: &str = "llm.enabled is off: no LLM advice is made";
const NO_ENDPOINT: &str = "llm.endpoint unavailable";

/// `llm.endpoint`, `llm.model`, `llm.num_ctx`, `llm.kanflow_gate`.
pub(super) async fn gateway_checks(cfg: &Config) -> [Check; 4] {
    let llm = &cfg.llm;
    let (true, Some(endpoint), Some(model)) =
        (llm.enabled, llm.endpoint.as_deref(), llm.model.as_ref())
    else {
        return NAMES.map(|name| skip(name, DISABLED));
    };
    let ca = match llm.ca_pem() {
        Ok(ca) => ca,
        Err(e) => {
            return [
                fail(ENDPOINT, e),
                skip(MODEL, NO_ENDPOINT),
                skip(NUM_CTX, NO_ENDPOINT),
                skip(KANFLOW_GATE, "llm.ca_file unreadable"),
            ]
        }
    };
    let gate = kanflow_check(llm, ca.as_deref()).await;
    let provider = match OllamaProvider::new(endpoint, ca.as_deref(), TIMEOUT) {
        Ok(p) => p,
        Err(e) => {
            return [
                fail(ENDPOINT, e),
                skip(MODEL, NO_ENDPOINT),
                skip(NUM_CTX, NO_ENDPOINT),
                gate,
            ]
        }
    };
    let digest = provider.digest(&model.name).await;
    let reached = digest.is_ok();
    let [endpoint_line, model_line] = model_lines(endpoint, model, llm.paused, digest);
    let num_ctx = if reached {
        num_ctx_line(&model.name, llm.num_ctx, provider.loaded(&model.name).await)
    } else {
        skip(NUM_CTX, NO_ENDPOINT)
    };
    [endpoint_line, model_line, num_ctx, gate]
}

/// `llm.endpoint` and `llm.model` from `/api/tags`'s answer for the model.
fn model_lines(
    endpoint: &str,
    model: &ModelRef,
    paused: bool,
    digest: Result<Option<String>, ForgeError>,
) -> [Check; 2] {
    let found = match digest {
        Ok(found) => found,
        Err(e) => {
            return [
                fail(ENDPOINT, e.with_context("endpoint", endpoint)),
                skip(MODEL, NO_ENDPOINT),
            ]
        }
    };
    let reached = ok(ENDPOINT, &format!("{endpoint} answers"));
    let model_line = match found {
        Some(d) if model.digest_matches(&d) => ok(
            MODEL,
            &format!("{model} present{}", if paused { " (paused)" } else { "" }),
        ),
        found => {
            let e = ForgeError::new(ErrorCode::LlmModelDigestMismatch)
                .with_context("model", &model.name)
                .with_context("expected_digest", &model.digest);
            fail(
                MODEL,
                match found {
                    Some(d) => e.with_context("actual_digest", d),
                    None => e.with_context("reason", "model_missing"),
                },
            )
        }
    };
    [reached, model_line]
}

/// The `reason` of an error, or `unknown`.
fn reason(e: &ForgeError) -> &str {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map_or("unknown", |(_, v)| v.as_str())
}

/// `llm.num_ctx` from `/api/ps`'s answer for the model.
fn num_ctx_line(model: &str, num_ctx: u32, loaded: Result<Loaded, ForgeError>) -> Check {
    match loaded {
        Ok(Loaded::NotLoaded) => ok(
            NUM_CTX,
            &format!("{model} not loaded now (llm.num_ctx {num_ctx}; compared while loaded)"),
        ),
        Ok(Loaded::Context(c)) if c == u64::from(num_ctx) => ok(
            NUM_CTX,
            &format!("{model} loaded with num_ctx {c}, the same as llm.num_ctx"),
        ),
        Ok(Loaded::Context(c)) => warn(
            NUM_CTX,
            &format!(
                "{model} loaded with num_ctx {c} but llm.num_ctx is {num_ctx}: the shared Ollama \
                 reloads the model whenever forge and Kanflow alternate; set Kanflow's value \
                 (runbook llm-gateway)"
            ),
        ),
        Ok(Loaded::Unknown) => warn(
            NUM_CTX,
            &format!(
                "this Ollama does not report the loaded context: compare llm.num_ctx \
                 ({num_ctx}) with Kanflow's by hand (runbook llm-gateway)"
            ),
        ),
        Err(e) => warn(
            NUM_CTX,
            &format!(
                "/api/ps not read (reason={}): llm.num_ctx ({num_ctx}) not compared \
                 (runbook llm-gateway)",
                reason(&e)
            ),
        ),
    }
}

/// `llm.kanflow_gate`.
async fn kanflow_check(llm: &LlmConfig, ca: Option<&[u8]>) -> Check {
    let Some(url) = llm.kanflow_busy_url.as_deref() else {
        return skip(
            KANFLOW_GATE,
            "llm.kanflow_busy_url not set: forge does not yield to Kanflow's users",
        );
    };
    let token = match llm.kanflow_token() {
        Ok(Some(token)) => token,
        // The configuration requires both (`needs_both_url_and_token`).
        Ok(None) => return skip(KANFLOW_GATE, "llm.kanflow_token_file not set"),
        Err(e) => return fail(KANFLOW_GATE, e),
    };
    match KanflowGate::new(url, token, ca, TIMEOUT) {
        Ok(gate) => gate_line(gate.probe().await),
        Err(e) => fail(KANFLOW_GATE, e),
    }
}

/// `llm.kanflow_gate` from one answer of Kanflow's `ai-busy`.
fn gate_line(answer: Result<Kanflow, ForgeError>) -> Check {
    const THROTTLED: &str = "the gateway counts Kanflow as idle and starts at most one \
                             request a minute (runbook llm-gateway)";
    match answer {
        Ok(Kanflow::Idle) => ok(KANFLOW_GATE, "idle: no Kanflow user waits"),
        Ok(Kanflow::Busy) => ok(
            KANFLOW_GATE,
            "busy: a Kanflow user waits (forge starts only interactive work)",
        ),
        Ok(Kanflow::Unreachable) => warn(
            KANFLOW_GATE,
            &format!("the answer is not Kanflow's ai-busy: {THROTTLED}"),
        ),
        Err(e) => warn(
            KANFLOW_GATE,
            &format!("no answer (reason={}): {THROTTLED}", reason(&e)),
        ),
    }
}

/// `llm.queue_age`.
pub(super) async fn queue_check(db: &Db, llm: &LlmConfig) -> Check {
    if !llm.enabled {
        return skip(QUEUE_AGE, DISABLED);
    }
    match forge_db::llm::queue_health(db).await {
        Ok(h) => queue_line(&h),
        Err(e) => fail(QUEUE_AGE, e),
    }
}

/// `llm.queue_age` from the records.
fn queue_line(h: &QueueHealth) -> Check {
    let overdue = h
        .oldest_queued_s
        .iter()
        .find(|(p, age)| u64::try_from(*age).unwrap_or(0) > (p.deadline() + QUEUE_GRACE).as_secs());
    if let Some((priority, age)) = overdue {
        return fail(
            QUEUE_AGE,
            ForgeError::new(ErrorCode::LlmQueueDeadline)
                .with_context("reason", "queued_past_deadline")
                .with_context("priority", priority)
                .with_context("age_s", age)
                .with_context("deadline_s", priority.deadline().as_secs())
                .with_context("queued", h.queued),
        );
    }
    let oldest = h
        .oldest_queued_s
        .iter()
        .map(|(p, age)| format!("{p} {age}s"))
        .collect::<Vec<_>>();
    let summary = format!(
        "{} queued, {} running{}",
        h.queued,
        h.running,
        if oldest.is_empty() {
            String::new()
        } else {
            format!(" (oldest: {})", oldest.join(", "))
        }
    );
    if h.deadline_misses > 0 {
        return warn(
            QUEUE_AGE,
            &format!(
                "{summary}; {} request(s) gave up waiting in the last hour [FG-LLM-0004] \
                 (runbook llm-gateway)",
                h.deadline_misses
            ),
        );
    }
    ok(QUEUE_AGE, &summary)
}

#[cfg(test)]
mod tests {
    use super::super::Outcome;
    use super::*;
    use forge_core::llm::Priority;

    fn model() -> ModelRef {
        ModelRef::parse(&format!("qwen3:27b@sha256:{}", "a".repeat(64))).expect("model")
    }

    #[test]
    fn the_endpoint_and_the_model_are_separate_lines() {
        let m = model();
        let [e, l] = model_lines("http://127.0.0.1:11434", &m, true, Ok(Some("a".repeat(64))));
        assert!(matches!(e.outcome, Outcome::Ok(_)));
        let Outcome::Ok(detail) = l.outcome else {
            panic!("not ok");
        };
        assert!(detail.contains("(paused)"), "{detail}");

        let [e, l] = model_lines(
            "http://127.0.0.1:11434",
            &m,
            false,
            Err(ForgeError::new(ErrorCode::LlmUnavailable).with_context("reason", "connect")),
        );
        let Outcome::Fail(err) = e.outcome else {
            panic!("not a failure");
        };
        assert_eq!(err.code(), ErrorCode::LlmUnavailable);
        assert!(matches!(l.outcome, Outcome::Skip(_)));

        for found in [Some("b".repeat(64)), None] {
            let [e, l] = model_lines("http://127.0.0.1:11434", &m, false, Ok(found));
            assert!(matches!(e.outcome, Outcome::Ok(_)));
            let Outcome::Fail(err) = l.outcome else {
                panic!("not a failure");
            };
            assert_eq!(err.code(), ErrorCode::LlmModelDigestMismatch);
        }
    }

    #[test]
    fn another_num_ctx_than_the_loaded_one_warns() {
        let line = |loaded| num_ctx_line("qwen3:27b", 8192, loaded).outcome;
        assert!(matches!(line(Ok(Loaded::NotLoaded)), Outcome::Ok(_)));
        assert!(matches!(line(Ok(Loaded::Context(8192))), Outcome::Ok(_)));
        let Outcome::Warn(detail) = line(Ok(Loaded::Context(4096))) else {
            panic!("not a warning");
        };
        assert!(
            detail.contains("4096") && detail.contains("8192"),
            "{detail}"
        );
        assert!(matches!(line(Ok(Loaded::Unknown)), Outcome::Warn(_)));
        let Outcome::Warn(detail) = line(Err(
            ForgeError::new(ErrorCode::LlmUnavailable).with_context("reason", "status_404")
        )) else {
            panic!("not a warning");
        };
        assert!(detail.contains("reason=status_404"), "{detail}");
    }

    #[test]
    fn an_unanswering_kanflow_gate_warns_never_fails() {
        assert!(matches!(
            gate_line(Ok(Kanflow::Idle)).outcome,
            Outcome::Ok(_)
        ));
        assert!(matches!(
            gate_line(Ok(Kanflow::Busy)).outcome,
            Outcome::Ok(_)
        ));
        assert!(matches!(
            gate_line(Ok(Kanflow::Unreachable)).outcome,
            Outcome::Warn(_)
        ));
        let Outcome::Warn(detail) = gate_line(Err(
            ForgeError::new(ErrorCode::LlmUnavailable).with_context("reason", "connect")
        ))
        .outcome
        else {
            panic!("not a warning");
        };
        assert!(detail.contains("reason=connect"), "{detail}");
    }

    #[test]
    fn a_request_queued_past_its_deadline_fails_with_the_deadline_code() {
        assert!(matches!(
            queue_line(&QueueHealth::default()).outcome,
            Outcome::Ok(_)
        ));
        // Within the deadline and its grace: ok, with the ages.
        let waiting = QueueHealth {
            queued: 2,
            running: 1,
            oldest_queued_s: vec![(Priority::Review, 30 * 60 + 60), (Priority::Batch, 3600)],
            deadline_misses: 0,
        };
        let Outcome::Ok(detail) = queue_line(&waiting).outcome else {
            panic!("not ok");
        };
        assert!(detail.contains("review 1860s"), "{detail}");
        // A miss in the last hour: warn.
        let missed = QueueHealth {
            deadline_misses: 3,
            ..waiting.clone()
        };
        let Outcome::Warn(detail) = queue_line(&missed).outcome else {
            panic!("not a warning");
        };
        assert!(detail.contains("FG-LLM-0004"), "{detail}");
        // Interactive 2 min + 2 min grace passed: nobody works the queue.
        let stuck = QueueHealth {
            queued: 1,
            oldest_queued_s: vec![(Priority::Interactive, 5 * 60)],
            ..QueueHealth::default()
        };
        let Outcome::Fail(e) = queue_line(&stuck).outcome else {
            panic!("not a failure");
        };
        assert_eq!(e.code(), ErrorCode::LlmQueueDeadline);
        assert!(e
            .context()
            .iter()
            .any(|(k, v)| *k == "reason" && v == "queued_past_deadline"));
    }
}
