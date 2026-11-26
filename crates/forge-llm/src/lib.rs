//! forge-llm: the local LLM gateway (plan 19 §1–§2, §8; ADR-0039).
//!
//! One [`Gateway`] per worker process asks the in-house Ollama (shared
//! with Kanflow) for advice. It decides nothing: callers get a checked
//! [`forge_core::llm::Advice`] (words only) or an error, never a success
//! made up from a failure.
//!
//! - [`Gateway::submit`]: record the request (`forge.llm_requests`, never
//!   the input), wait for a turn ([`sched`]), call the model with the
//!   checked retry ([`forge_core::llm::generate_checked`]), write the advice
//!   (`forge.llm_advice`) and finish the record. The gateway is the
//!   boundary that **logs** its failures (once, with `request_id` and
//!   `error_code`); callers act on the returned code without logging it
//!   again.
//! - Failures: `FG-LLM-0001` (Ollama unreachable, slow, out of memory),
//!   `FG-LLM-0002` (invalid answer twice), `FG-LLM-0003` (the model's
//!   digest is not the pinned one: inference stops until it is),
//!   `FG-LLM-0004` (the request's deadline passed in the queue),
//!   `FG-LLM-0006` (findings dropped for their evidence, info). Paused:
//!   [`Outcome::Paused`], nothing queued, no code (plan 19 §8.1).
//! - [`background::spawn`]: the clock (promotions, throttling), Kanflow's
//!   `ai-busy` ([`kanflow`]), whether a `gpu-eval` job runs, the model
//!   digest every 10 min, and a metrics snapshot ([`metrics`]) every
//!   5 min.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use forge_core::llm::{
    generate_checked, Advice, ChatRequest, FindingSeverity, LlmProvider, ModelRef, OutputPolicy,
    PreparedInput, Priority, Purpose, SubjectKind,
};
use forge_core::{telemetry, ProjectId, RequestId, UserId};
use forge_db::llm::{Ending, NewAdvice, NewRequest, Usage};
use forge_errors::{ErrorCode, ForgeError};
use tokio::sync::oneshot;
use tokio::time::Instant;

pub mod background;
pub mod diagnoses;
pub mod kanflow;
pub mod metrics;
pub mod ollama;
pub mod prompts;
pub mod records;
pub mod sched;

use metrics::{label, Metrics};
pub use records::{DbRecords, RecordFuture, Records};
use sched::{Conditions, Kanflow, Scheduler};

/// The boxed future of [`ModelCatalog::digest`].
pub type DigestFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<String>, ForgeError>> + Send + 'a>>;

/// Where the model's actual digest is read (Ollama's `/api/tags`).
pub trait ModelCatalog: Send + Sync {
    /// The digest of `model` (`name:tag`); `None`: the model is not there.
    fn digest<'a>(&'a self, model: &'a str) -> DigestFuture<'a>;
}

/// The gateway's settings (from `llm.*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayConfig {
    pub model: ModelRef,
    pub num_ctx: u32,
    pub concurrency: u8,
    pub paused: bool,
    /// One model call's timeout (`llm.timeout_ms`); a whole checked call
    /// (two attempts) is bounded by twice this plus 5 s.
    pub call_timeout: Duration,
}

/// A versioned prompt (`crates/forge-llm/prompts/<purpose>.v<N>`, L5-5+).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub id: String,
    pub version: i32,
    pub system: String,
}

/// One request, as a feature builds it.
#[derive(Debug, Clone)]
pub struct Submission {
    pub purpose: Purpose,
    /// `None`: the purpose's tier.
    pub priority: Option<Priority>,
    pub subject_kind: SubjectKind,
    pub subject_id: Option<i64>,
    pub project: Option<ProjectId>,
    pub requested_by: Option<UserId>,
    pub head_sha: Option<String>,
    pub prompt: Prompt,
    pub input: PreparedInput,
    pub policy: OutputPolicy,
    pub request_id: RequestId,
}

/// A written advice.
#[derive(Debug, Clone, PartialEq)]
pub struct Advised {
    pub llm_request_id: i64,
    pub advice_id: i64,
    pub advice: Advice,
    pub attempts: u8,
}

/// What [`Gateway::submit`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Advised(Box<Advised>),
    /// The gateway is paused: nothing was queued (the screens say "paused").
    Paused,
}

enum Turn {
    Go,
    Stopped,
}

struct State {
    sched: Scheduler,
    tickets: HashMap<u64, oneshot::Sender<Turn>>,
    /// Tickets already counted in the yield metric.
    yielded: HashSet<u64>,
    next_ticket: u64,
    paused: bool,
    /// The model digest did not match (FG-LLM-0003): nothing runs.
    stopped: bool,
    cond: Conditions,
}

struct Inner {
    cfg: GatewayConfig,
    provider: Arc<dyn LlmProvider>,
    records: Arc<dyn Records>,
    metrics: Metrics,
    state: Mutex<State>,
}

/// The local LLM gateway (module docs). Cheap to clone.
#[derive(Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("model", &self.inner.cfg.model.name)
            .finish_non_exhaustive()
    }
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    // The state stays consistent between statements; a panic elsewhere
    // while it was held does not make it unusable.
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn digest_mismatch(model: &ModelRef) -> ForgeError {
    ForgeError::new(ErrorCode::LlmModelDigestMismatch)
        .with_context("model", &model.name)
        .with_context("expected_digest", &model.digest)
}

impl Gateway {
    pub fn new(
        cfg: GatewayConfig,
        provider: Arc<dyn LlmProvider>,
        records: Arc<dyn Records>,
    ) -> Self {
        let state = State {
            sched: Scheduler::new(usize::from(cfg.concurrency)),
            tickets: HashMap::new(),
            yielded: HashSet::new(),
            next_ticket: 1,
            paused: cfg.paused,
            stopped: false,
            cond: Conditions::default(),
        };
        Self {
            inner: Arc::new(Inner {
                cfg,
                provider,
                records,
                metrics: Metrics::default(),
                state: Mutex::new(state),
            }),
        }
    }

    pub fn metrics(&self) -> &Metrics {
        &self.inner.metrics
    }

    pub fn config(&self) -> &GatewayConfig {
        &self.inner.cfg
    }

    /// Stop queueing new requests (queued and running ones go on).
    pub fn pause(&self) {
        lock(&self.inner.state).paused = true;
    }

    pub fn resume(&self) {
        lock(&self.inner.state).paused = false;
        self.dispatch();
    }

    pub fn is_paused(&self) -> bool {
        lock(&self.inner.state).paused
    }

    /// The model digest did not match: nothing runs until it does.
    pub fn is_stopped(&self) -> bool {
        lock(&self.inner.state).stopped
    }

    /// (waiting, running) requests.
    pub fn queue_len(&self) -> (usize, usize) {
        let s = lock(&self.inner.state);
        (s.sched.waiting(), s.sched.running())
    }

    /// New outside conditions (`None`: unchanged); waiting work may start.
    pub fn set_conditions(&self, kanflow: Option<Kanflow>, gpu_eval_running: Option<bool>) {
        {
            let mut s = lock(&self.inner.state);
            if let Some(k) = kanflow {
                s.cond.kanflow = k;
            }
            if let Some(g) = gpu_eval_running {
                s.cond.gpu_eval_running = g;
            }
        }
        self.dispatch();
    }

    /// The clock moved (promotions, the unreachable-Kanflow throttle).
    pub fn tick(&self) {
        self.dispatch();
    }

    /// Check the model's digest. A mismatch (or a missing model) stops
    /// inference and fails every waiting request (`FG-LLM-0003`); a match
    /// starts it again. Unreachable: `FG-LLM-0001`, nothing changes. The
    /// caller logs the error (once per streak).
    pub async fn verify_model(&self, catalog: &dyn ModelCatalog) -> Result<(), ForgeError> {
        let model = &self.inner.cfg.model;
        let reported = catalog.digest(&model.name).await?;
        let matches = reported.as_deref().is_some_and(|d| model.digest_matches(d));
        let drained = {
            let mut s = lock(&self.inner.state);
            s.stopped = !matches;
            if matches {
                Vec::new()
            } else {
                let tickets = s.sched.drain();
                tickets
                    .into_iter()
                    .filter_map(|t| s.tickets.remove(&t))
                    .collect()
            }
        };
        for tx in drained {
            // The waiting caller may be gone; nothing else to tell.
            let _gone = tx.send(Turn::Stopped);
        }
        if matches {
            self.dispatch();
            Ok(())
        } else {
            let mut e = digest_mismatch(model);
            e = match reported {
                Some(d) => e.with_context("actual_digest", d),
                None => e.with_context("reason", "model_missing"),
            };
            Err(e)
        }
    }

    /// Hand turns to waiting tickets while slots are free.
    fn dispatch(&self) {
        let now = Instant::now();
        let mut go = Vec::new();
        let mut yields = Vec::new();
        {
            let mut s = lock(&self.inner.state);
            if s.stopped {
                return;
            }
            loop {
                let cond = s.cond;
                let (picked, held) = s.sched.pick(now, cond);
                for (ticket, why) in held {
                    if s.yielded.insert(ticket) {
                        yields.push(why);
                    }
                }
                let Some(ticket) = picked else { break };
                s.yielded.remove(&ticket);
                match s.tickets.remove(&ticket) {
                    Some(tx) => go.push((ticket, tx)),
                    // Cannot happen (a ticket waits only with its sender);
                    // free the slot it was given.
                    None => s.sched.finished(Duration::ZERO, false, now),
                }
            }
        }
        for why in yields {
            self.inner
                .metrics
                .add(metrics::YIELDS, label(&[("reason", why.as_str())]), 1);
        }
        let mut released = false;
        for (_ticket, tx) in go {
            if tx.send(Turn::Go).is_err() {
                // The caller gave up at this very moment: free its slot.
                lock(&self.inner.state)
                    .sched
                    .finished(Duration::ZERO, false, now);
                released = true;
            }
        }
        if released {
            self.dispatch();
        }
    }

    fn release(&self, busy: Duration, review: bool) {
        lock(&self.inner.state)
            .sched
            .finished(busy, review, Instant::now());
        self.dispatch();
    }

    fn log(&self, e: &ForgeError, s: &Submission, id: Option<i64>) {
        let mut e = copy(e)
            .with_context("event", "forge.llm.request_finished")
            .with_context("purpose", s.purpose)
            .with_context("model", &self.inner.cfg.model.name)
            .with_context("prompt_version", s.prompt.version);
        if let Some(id) = id {
            e = e.with_context("llm_request_id", id);
        }
        telemetry::log_failure(&e, &s.request_id);
    }

    fn count(&self, purpose: Purpose, outcome: &str) {
        self.inner.metrics.add(
            metrics::REQUESTS,
            label(&[("purpose", purpose.as_str()), ("outcome", outcome)]),
            1,
        );
    }

    async fn end(&self, id: i64, ending: Ending, usage: Usage, s: &Submission) {
        if let Err(e) = self.inner.records.finished(id, ending, usage).await {
            self.log(&e.with_context("step", "finish_record"), s, Some(id));
        }
    }

    /// Ask for an advice (module docs).
    pub async fn submit(&self, s: Submission) -> Result<Outcome, ForgeError> {
        let priority = s.priority.unwrap_or(s.purpose.priority());
        {
            let st = lock(&self.inner.state);
            if st.paused {
                tracing::info!(
                    target: "forge_llm",
                    event = "forge.llm.paused_skip",
                    request_id = %s.request_id,
                    purpose = %s.purpose,
                    "the LLM gateway is paused; nothing was queued"
                );
                return Ok(Outcome::Paused);
            }
            if st.stopped {
                let e = digest_mismatch(&self.inner.cfg.model).with_context("reason", "stopped");
                drop(st);
                self.count(s.purpose, e.code().id());
                return Err(e);
            }
        }
        let record = NewRequest {
            purpose: s.purpose,
            priority,
            subject_kind: s.subject_kind,
            subject_id: s.subject_id,
            project: s.project,
            requested_by: s.requested_by,
            model: self.inner.cfg.model.name.clone(),
            model_digest: Some(self.inner.cfg.model.digest.clone()),
            prompt_id: s.prompt.id.clone(),
            prompt_version: s.prompt.version,
            input_sha256: s.input.sha256().to_owned(),
            input_class: s.input.class(),
            request_id: s.request_id.clone(),
        };
        let id = match self.inner.records.queued(&record).await {
            Ok(id) => id,
            Err(e) => {
                let e = e.with_context("step", "queue_record");
                self.log(&e, &s, None);
                return Err(e);
            }
        };
        let (tx, rx) = oneshot::channel();
        let queued_at = Instant::now();
        let ticket = {
            let mut st = lock(&self.inner.state);
            let ticket = st.next_ticket;
            st.next_ticket += 1;
            st.sched.enqueue(ticket, priority, queued_at);
            st.tickets.insert(ticket, tx);
            ticket
        };
        self.dispatch();
        let turn = tokio::time::timeout(priority.deadline(), rx).await;
        match turn {
            Ok(Ok(Turn::Go)) => {}
            Ok(Ok(Turn::Stopped)) | Ok(Err(_)) => {
                let e = digest_mismatch(&self.inner.cfg.model).with_context("reason", "stopped");
                self.end(id, Ending::Abandoned(e.code()), Usage::default(), &s)
                    .await;
                self.count(s.purpose, e.code().id());
                self.log(&e, &s, Some(id));
                return Err(e);
            }
            Err(_elapsed) => {
                let given = {
                    let mut st = lock(&self.inner.state);
                    st.yielded.remove(&ticket);
                    if st.sched.cancel(ticket) {
                        st.tickets.remove(&ticket);
                        false
                    } else {
                        true
                    }
                };
                if given {
                    // Its turn came as the deadline passed: free the slot.
                    self.release(Duration::ZERO, false);
                }
                let e = ForgeError::new(ErrorCode::LlmQueueDeadline)
                    .with_context("priority", priority)
                    .with_context("deadline_s", priority.deadline().as_secs());
                self.end(id, Ending::Abandoned(e.code()), Usage::default(), &s)
                    .await;
                self.count(s.purpose, e.code().id());
                self.log(&e, &s, Some(id));
                return Err(e);
            }
        }
        self.inner.metrics.observe(
            metrics::QUEUE_WAIT,
            label(&[("priority", priority.as_str())]),
            queued_at.elapsed(),
        );
        let started = Instant::now();
        let result = self.run(id, &s).await;
        self.release(started.elapsed(), s.purpose == Purpose::MrReview);
        self.inner.metrics.observe(
            metrics::REQUEST_DURATION,
            label(&[("purpose", s.purpose.as_str())]),
            started.elapsed(),
        );
        match result {
            Ok(advised) => {
                self.count(s.purpose, "ok");
                Ok(Outcome::Advised(Box::new(advised)))
            }
            Err((e, usage)) => {
                if e.code() == ErrorCode::LlmOutputInvalid {
                    self.inner.metrics.add(
                        metrics::OUTPUT_INVALID,
                        label(&[("purpose", s.purpose.as_str())]),
                        1,
                    );
                }
                self.end(id, Ending::Failed(e.code()), usage, &s).await;
                self.count(s.purpose, e.code().id());
                self.log(&e, &s, Some(id));
                Err(e)
            }
        }
    }

    /// The turn: the model call and the advice row.
    async fn run(&self, id: i64, s: &Submission) -> Result<Advised, (ForgeError, Usage)> {
        let mut usage = Usage {
            attempts: 1,
            input_tokens: None,
            output_tokens: None,
        };
        self.inner
            .records
            .running(id)
            .await
            .map_err(|e| (e.with_context("step", "running_record"), usage))?;
        let chat = ChatRequest {
            model: self.inner.cfg.model.name.clone(),
            system: s.prompt.system.clone(),
            user: s.input.render(),
            num_ctx: self.inner.cfg.num_ctx,
        };
        let bound = self.inner.cfg.call_timeout * 2 + Duration::from_secs(5);
        let generated = match tokio::time::timeout(
            bound,
            generate_checked(&*self.inner.provider, &chat, &s.input, &s.policy),
        )
        .await
        {
            Ok(Ok(g)) => g,
            Ok(Err(e)) => {
                if e.code() == ErrorCode::LlmOutputInvalid {
                    usage.attempts = 2;
                }
                return Err((e, usage));
            }
            Err(_) => {
                return Err((
                    ForgeError::new(ErrorCode::LlmUnavailable)
                        .with_context("reason", "gateway_timeout")
                        .with_context("timeout_s", bound.as_secs()),
                    usage,
                ))
            }
        };
        usage = Usage {
            attempts: i16::from(generated.attempts),
            input_tokens: i32::try_from(generated.input_tokens).ok(),
            output_tokens: i32::try_from(generated.output_tokens).ok(),
        };
        self.inner.metrics.add(
            metrics::TOKENS,
            label(&[("direction", "input")]),
            u64::from(generated.input_tokens),
        );
        self.inner.metrics.add(
            metrics::TOKENS,
            label(&[("direction", "output")]),
            u64::from(generated.output_tokens),
        );
        let advice = generated.advice;
        if let Some(note) = advice.evidence_note() {
            self.inner.metrics.add(
                metrics::EVIDENCE_DROPPED,
                label(&[("purpose", s.purpose.as_str())]),
                u64::try_from(advice.dropped_evidence).unwrap_or(u64::MAX),
            );
            self.log(&note, s, Some(id));
        }
        let row = NewAdvice {
            llm_request_id: Some(id),
            purpose: s.purpose,
            subject_kind: s.subject_kind,
            subject_id: s.subject_id,
            project: s.project,
            head_sha: s.head_sha.clone(),
            summary: advice.summary.clone(),
            findings: findings_json(&advice),
            deep_review: advice.deep_review.as_ref().map(|d| d.reason.clone()),
            dropped_findings: i32::try_from(
                advice.dropped_evidence
                    + advice.dropped_low_confidence
                    + advice.dropped_over_cap
                    + advice.dropped_mutating,
            )
            .unwrap_or(i32::MAX),
            degraded: advice.degraded,
            input_class: advice.input_class,
            model: self.inner.cfg.model.name.clone(),
            prompt_version: s.prompt.version,
        };
        let advice_id = self
            .inner
            .records
            .advice(&row)
            .await
            .map_err(|e| (e.with_context("step", "advice_record"), usage))?;
        self.end(id, Ending::Succeeded, usage, s).await;
        tracing::info!(
            target: "forge_llm",
            event = "forge.llm.request_finished",
            request_id = %s.request_id,
            llm_request_id = id,
            advice_id,
            purpose = %s.purpose,
            outcome = "ok",
            attempts = generated.attempts,
            findings = advice.findings.len(),
            input_class = %advice.input_class,
            "an LLM advice was written"
        );
        Ok(Advised {
            llm_request_id: id,
            advice_id,
            advice,
            attempts: generated.attempts,
        })
    }
}

/// `e`'s code and context (and its source as text), to log while `e`
/// itself goes back to the caller (`ForgeError` is not `Clone`).
pub(crate) fn copy(e: &ForgeError) -> ForgeError {
    let mut c = ForgeError::new(e.code());
    for (k, v) in e.context() {
        c = c.with_context(k, v);
    }
    if let Some(src) = std::error::Error::source(e) {
        c = c.with_context("source", src);
    }
    c
}

/// The findings as stored (`forge.llm_advice.findings`).
pub fn findings_json(advice: &Advice) -> serde_json::Value {
    serde_json::Value::Array(
        advice
            .findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "severity": severity(f.severity),
                    "path": f.path,
                    "line": f.line,
                    "quote": f.quote,
                    "message": f.message,
                    "confidence": f.confidence,
                })
            })
            .collect(),
    )
}

fn severity(s: FindingSeverity) -> &'static str {
    s.as_str()
}

#[cfg(test)]
mod tests;
