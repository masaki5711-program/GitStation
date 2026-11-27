//! The merge request first pass (M5 L5-5, plan 19 §3.1, ADR-0039
//! Amendment L5-5): every [`PERIOD`], while the gateway runs (`llm.enabled`)
//! and is neither paused nor stopped, the open non-draft merge requests of
//! projects whose `llm.review` is not `off` whose head has no
//! `forge/ai-review` run yet (`forge_db::llm::review::review_candidates`,
//! at most 20 a pass, at most [`MAX_IN_FLIGHT`] at once) are each reviewed
//! by their own task ([`review_one`]):
//!
//! 1. the input is read ([`ReviewSource`]: the merge request, the diff of
//!    its version `merge base..head`, the confidential paths at both ends —
//!    a failure to read those makes the whole input confidential, fail
//!    closed) and assembled by `forge_review::ai_review::assemble`;
//! 2. nothing readable (only lock, generated or binary files) → `neutral`
//!    without asking; an input refused (`FG-LLM-0005`) → `error`;
//! 3. the run is `queued`, the gateway is asked (review tier, the
//!    `mr_review` prompt, the code rules' deep-review reasons);
//! 4. the answer is written only if the merge request still has that head
//!    and is open — otherwise the old head's run is `neutral` "moved on"
//!    and the new head is a candidate of its own (`stale_head_result_
//!    discarded`); a failure of the LLM is `error` with its code and
//!    request id (`llm_down_sets_error_not_success`); paused → the run says
//!    it waits.
//!
//! Nothing here blocks or approves a merge: `forge/ai-review` is no
//! required check unless a Maintainer names it in a branch rule, and only
//! `required` projects ever see `failure`. Off projects get no row at all
//! (`off_project_gets_no_check_row`). A run left `queued` by a stopped
//! worker is asked again after [`STALE_WAIT_SECS`]. Disabled LLM: no task,
//! no row.
//!
//! The gateway logs its own failures; this task logs what it decided
//! (`forge.review.ai_review_recorded`) and its own failures (a read, a
//! refused input, a write) once each with `task=ai_review`.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_checks::confidential::{confidential_at, Confidential, ConfidentialLimits};
use forge_core::checks::CheckConclusion;
use forge_core::llm::{prepare, InputLimits, Purpose, SubjectKind};
use forge_core::{telemetry, ObjectId, ProjectAccessLoader as _, RepositoryState, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::NewCheckRun;
use forge_db::llm::review::{ReviewCandidate, MAX_REVIEW_CANDIDATES};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, Diff};
use forge_llm::{Gateway, Outcome, Submission};
use forge_review::ai_review::{self as rules, RunLabel};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

/// The task name in the logs.
pub(crate) const NAME: &str = "ai_review";
/// How often it looks for heads to review.
pub(crate) const PERIOD: Duration = Duration::from_secs(30);
/// A `queued` run older than this is asked again (a review waits 30 min at
/// most in the gateway, plus the call).
pub(crate) const STALE_WAIT_SECS: i64 = 40 * 60;
/// Reviews waiting in the gateway at once (the gateway runs one at a time).
pub(crate) const MAX_IN_FLIGHT: usize = 20;
/// One write of the run.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Reading the input (the merge request, the diff, the marks).
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// Which paths are confidential at the merge base or the head.
pub(crate) enum Marks {
    Known(Confidential),
    /// They could not be read: every path is (fail closed).
    All,
}

impl Marks {
    fn contains(&self, path: &str) -> bool {
        match self {
            Self::Known(c) => c.contains(path),
            Self::All => true,
        }
    }
}

/// The input of one head, as read.
pub(crate) struct Loaded {
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) diff: Diff,
    pub(crate) marks: Marks,
}

/// What reading a candidate found.
pub(crate) enum Load {
    Ready(Box<Loaded>),
    /// The version job has not recorded the head's merge base yet: asked
    /// again on a later pass (no row).
    NotReady,
    /// The merge request moved on (or closed) since the candidates were read.
    Moved,
    /// The head shares no history with the target.
    NoCommonHistory,
}

/// The boxed future of [`ReviewSource::load`].
pub(crate) type LoadFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Load, ForgeError>> + Send + 'a>>;

/// Where the input of a head is read (git in the worker; scripted in tests).
pub(crate) trait ReviewSource: Send + Sync {
    fn load<'a>(&'a self, c: &'a ReviewCandidate, request_id: &'a RequestId) -> LoadFuture<'a>;
}

/// The worker's source: the database and the repository's git.
pub(crate) struct GitSource {
    pub(crate) db: Db,
    pub(crate) browse: Browser,
}

impl ReviewSource for GitSource {
    fn load<'a>(&'a self, c: &'a ReviewCandidate, request_id: &'a RequestId) -> LoadFuture<'a> {
        Box::pin(async move {
            let Some(mr) = forge_db::review::get(&self.db, c.project, c.iid).await? else {
                return Ok(Load::Moved);
            };
            if mr.head_sha.as_deref() != Some(c.head_sha.as_str())
                || mr.state != forge_core::MergeRequestState::Open
            {
                return Ok(Load::Moved);
            }
            let versions = forge_db::review::read_versions(&self.db, mr.id).await?;
            let Some(version) = versions
                .iter()
                .rev()
                .find(|v| v.head_sha.as_str() == c.head_sha)
            else {
                return Ok(Load::NotReady);
            };
            let Some(base) = &version.merge_base_sha else {
                return Ok(if version.base_sha.is_some() {
                    Load::NoCommonHistory
                } else {
                    Load::NotReady
                });
            };
            let Some(record) = self.db.load_project(c.project, None).await? else {
                return Ok(Load::Moved);
            };
            let repo = record.repository;
            if repo.state != RepositoryState::Ready {
                return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                    .with_context("repo_id", repo.id.get())
                    .with_context("state", repo.state.as_str()));
            }
            let compared = self
                .browse
                .compare(repo.id, base.as_str(), &c.head_sha)
                .await?;
            let limits = ConfidentialLimits::default();
            let mut marks = Confidential::default();
            let mut failed = None;
            for commit in [base.as_str(), c.head_sha.as_str()] {
                match confidential_at(&self.browse, repo.id, commit, &limits).await {
                    Ok(found) => marks.extend(found),
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            let marks = match failed {
                None => Marks::Known(marks),
                Some(e) => {
                    // Fail closed: the advice is shown to people only.
                    telemetry::log_failure(
                        &e.with_context("task", NAME)
                            .with_context("step", "confidential_paths")
                            .with_context("fallback", "all_confidential")
                            .with_context("merge_request_id", c.mr.get()),
                        request_id,
                    );
                    Marks::All
                }
            };
            Ok(Load::Ready(Box::new(Loaded {
                title: mr.title,
                description: mr.description,
                diff: compared.diff,
                marks,
            })))
        })
    }
}

/// What one review did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reviewed {
    /// The run was written with this conclusion (an advice, or `neutral`
    /// without asking: nothing readable, no common history).
    Recorded(CheckConclusion),
    /// The LLM (or the input) failed: `error`.
    Failed(ErrorCode),
    /// The merge request moved on: nothing (or "moved on") written.
    Discarded,
    /// Not ready yet: no row, asked on a later pass.
    NotReady,
    /// The gateway is paused: the run says it waits.
    Paused,
}

fn timed_out() -> ForgeError {
    ForgeError::new(ErrorCode::RequestTimeout)
        .with_context("operation", "ai_review_record")
        .with_context("timeout_ms", WRITE_TIMEOUT.as_millis())
}

async fn record(
    db: &Db,
    c: &ReviewCandidate,
    request_id: &RequestId,
    run: &NewCheckRun,
) -> Result<(), ForgeError> {
    let head = ObjectId::parse(&c.head_sha).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "candidate_head_invalid")
    })?;
    let ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the change owns its id
    let write = forge_db::checks::record_forge_check_run(db, &ctx, (c.project, &head), run);
    match tokio::time::timeout(WRITE_TIMEOUT, write).await {
        Ok(written) => {
            written?;
            Ok(())
        }
        Err(_) => Err(timed_out()),
    }
}

fn logged(
    e: ForgeError,
    c: &ReviewCandidate,
    request_id: &RequestId,
    step: &'static str,
) -> ForgeError {
    let e = e
        .with_context("task", NAME)
        .with_context("step", step)
        .with_context("merge_request_id", c.mr.get())
        .with_context("project_id", c.project.get());
    telemetry::log_failure(&e, request_id);
    e
}

fn recorded(
    c: &ReviewCandidate,
    request_id: &RequestId,
    outcome: &str,
    conclusion: Option<CheckConclusion>,
) {
    tracing::info!(
        target: "forge_cli",
        event = "forge.review.ai_review_recorded",
        request_id = %request_id,
        merge_request_id = c.mr.get(),
        project_id = c.project.get(),
        head_sha = %c.head_sha,
        outcome,
        conclusion = conclusion.map(|k| k.as_str()),
        "the AI first pass of a merge request head was recorded"
    );
}

/// Whether the merge request still has `c`'s head and is open.
async fn still_current(db: &Db, c: &ReviewCandidate) -> Result<bool, ForgeError> {
    Ok(matches!(
        forge_db::llm::review::current_head(db, c.mr).await?,
        Some((Some(head), true)) if head == c.head_sha
    ))
}

/// Review one head (module docs). `Err` only when the run could not be
/// written (already logged); the LLM's failure is `Ok(Failed)`.
pub(crate) async fn review_one(
    gateway: &Gateway,
    db: &Db,
    source: &dyn ReviewSource,
    c: &ReviewCandidate,
    request_id: &RequestId,
) -> Result<Reviewed, ForgeError> {
    let loaded = match tokio::time::timeout(LOAD_TIMEOUT, source.load(c, request_id)).await {
        Ok(Ok(load)) => load,
        Ok(Err(e)) => {
            let e = logged(e, c, request_id, "load");
            let code = e.code();
            write(
                db,
                c,
                request_id,
                &rules::failed_run(code, request_id.as_str())?,
            )
            .await?;
            recorded(c, request_id, "load_failed", Some(CheckConclusion::Error));
            return Ok(Reviewed::Failed(code));
        }
        Err(_) => {
            let e = logged(
                ForgeError::new(ErrorCode::RequestTimeout)
                    .with_context("operation", "ai_review_load")
                    .with_context("timeout_ms", LOAD_TIMEOUT.as_millis()),
                c,
                request_id,
                "load",
            );
            let code = e.code();
            write(
                db,
                c,
                request_id,
                &rules::failed_run(code, request_id.as_str())?,
            )
            .await?;
            recorded(c, request_id, "load_failed", Some(CheckConclusion::Error));
            return Ok(Reviewed::Failed(code));
        }
    };
    let loaded = match loaded {
        Load::Ready(l) => l,
        Load::NotReady => return Ok(Reviewed::NotReady),
        Load::Moved => return Ok(Reviewed::Discarded),
        Load::NoCommonHistory => {
            write(db, c, request_id, &rules::no_common_history_run()?).await?;
            recorded(
                c,
                request_id,
                "no_common_history",
                Some(CheckConclusion::Neutral),
            );
            return Ok(Reviewed::Recorded(CheckConclusion::Neutral));
        }
    };
    let assembled = rules::assemble(&loaded.title, &loaded.description, &loaded.diff, |p| {
        loaded.marks.contains(p)
    });
    if assembled.reviewed == 0 {
        write(
            db,
            c,
            request_id,
            &rules::nothing_to_review_run(&assembled.excluded, assembled.excluded_marked)?,
        )
        .await?;
        recorded(
            c,
            request_id,
            "nothing_to_review",
            Some(CheckConclusion::Neutral),
        );
        return Ok(Reviewed::Recorded(CheckConclusion::Neutral));
    }
    let input = match prepare(assembled.parts.clone(), &InputLimits::default()) {
        // clone: the run's summary reads the assembly afterwards
        Ok(input) => input,
        Err(e) => {
            let e = logged(e, c, request_id, "prepare");
            let code = e.code();
            write(
                db,
                c,
                request_id,
                &rules::failed_run(code, request_id.as_str())?,
            )
            .await?;
            recorded(c, request_id, "input_refused", Some(CheckConclusion::Error));
            return Ok(Reviewed::Failed(code));
        }
    };
    write(db, c, request_id, &rules::waiting_run(false)?).await?;
    let deep = rules::deep_review_text(&assembled.deep_review);
    let submitted = gateway
        .submit(Submission {
            purpose: Purpose::MrReview,
            priority: None,
            subject_kind: SubjectKind::MergeRequest,
            subject_id: Some(c.mr.get()),
            project: Some(c.project),
            requested_by: None,
            head_sha: Some(c.head_sha.clone()), // clone: the advice binds to it
            deep_review: deep.clone(),          // clone: the run shows it too
            prompt: forge_llm::prompts::mr_review(),
            input,
            policy: forge_llm::prompts::mr_review_policy(),
            request_id: request_id.clone(), // clone: the submission owns it
        })
        .await;
    match submitted {
        Ok(Outcome::Paused) => {
            write(db, c, request_id, &rules::waiting_run(true)?).await?;
            recorded(c, request_id, "paused", None);
            Ok(Reviewed::Paused)
        }
        Ok(Outcome::Advised(a)) => {
            if !still_current(db, c).await? {
                write(db, c, request_id, &rules::superseded_run()?).await?;
                recorded(
                    c,
                    request_id,
                    "stale_head_discarded",
                    Some(CheckConclusion::Neutral),
                );
                return Ok(Reviewed::Discarded);
            }
            let model = &gateway.config().model.name;
            let shown_deep = forge_llm::deep_review(deep.as_deref(), a.advice.deep_review.as_ref());
            let run = rules::advised_run(
                c.mode,
                &a.advice,
                &assembled,
                shown_deep.as_deref(),
                &RunLabel {
                    model,
                    prompt_version: forge_llm::prompts::mr_review().version,
                    advice_id: a.advice_id,
                    request_id: request_id.as_str(),
                },
            )?;
            let conclusion = run.conclusion.unwrap_or(CheckConclusion::Error);
            write(db, c, request_id, &run).await?;
            recorded(c, request_id, "advised", Some(conclusion));
            Ok(Reviewed::Recorded(conclusion))
        }
        Err(e) => {
            // The gateway logged it.
            let code = e.code();
            if !still_current(db, c).await? {
                write(db, c, request_id, &rules::superseded_run()?).await?;
                recorded(
                    c,
                    request_id,
                    "stale_head_discarded",
                    Some(CheckConclusion::Neutral),
                );
                return Ok(Reviewed::Discarded);
            }
            write(
                db,
                c,
                request_id,
                &rules::failed_run(code, request_id.as_str())?,
            )
            .await?;
            recorded(c, request_id, "llm_failed", Some(CheckConclusion::Error));
            Ok(Reviewed::Failed(code))
        }
    }
}

/// Write `run`, logging a failure once.
async fn write(
    db: &Db,
    c: &ReviewCandidate,
    request_id: &RequestId,
    run: &NewCheckRun,
) -> Result<(), ForgeError> {
    record(db, c, request_id, run)
        .await
        .map_err(|e| logged(e, c, request_id, "record"))
}

/// The heads one pass starts (not running already), at most
/// [`MAX_IN_FLIGHT`] in all. Nothing while the gateway is paused or stopped
/// (the model's digest is wrong): nothing is queued then.
pub(crate) async fn candidates(
    gateway: &Gateway,
    db: &Db,
    in_flight: &InFlight,
) -> Result<Vec<ReviewCandidate>, ForgeError> {
    if gateway.is_paused() || gateway.is_stopped() {
        return Ok(Vec::new());
    }
    let found =
        forge_db::llm::review::review_candidates(db, MAX_REVIEW_CANDIDATES, STALE_WAIT_SECS)
            .await?;
    let mut running = in_flight.lock();
    let mut out = Vec::new();
    for c in found {
        if running.len() >= MAX_IN_FLIGHT {
            break;
        }
        if running.insert((c.mr.get(), c.head_sha.clone())) {
            // clone: the set keys by an owned head
            out.push(c);
        }
    }
    Ok(out)
}

/// The heads being reviewed now (this process). A poisoned lock is still
/// read (the set is only keys): a panicking review never blocks the others.
#[derive(Debug, Default)]
pub(crate) struct InFlight(Mutex<HashSet<(i64, String)>>);

impl InFlight {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<(i64, String)>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn clear(&self) {
        self.lock().clear();
    }
}

/// Removes its head from [`InFlight`] when the review ends — also when it
/// panics or is aborted at shutdown.
struct Running {
    set: Arc<InFlight>,
    key: (i64, String),
}

impl Drop for Running {
    fn drop(&mut self) {
        self.set.lock().remove(&self.key);
    }
}

/// Run a pass every [`PERIOD`] until `stop`; each head in its own task of
/// one [`JoinSet`], aborted on `stop` (a review cut there leaves its
/// `queued` run, asked again after [`STALE_WAIT_SECS`]). A task that
/// panicked is logged once with `task=ai_review`.
pub(crate) fn spawn(
    gateway: Gateway,
    db: Db,
    source: Arc<dyn ReviewSource>,
    mut stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let in_flight: Arc<InFlight> = Arc::default();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut failing: Option<ErrorCode> = None;
        loop {
            if *stop.borrow() {
                tasks.abort_all();
                return;
            }
            let request_id = RequestId::generate();
            while let Some(done) = tasks.try_join_next() {
                if let Err(e) = done {
                    if e.is_panic() {
                        telemetry::log_failure(
                            &ForgeError::new(ErrorCode::Internal)
                                .with_context("task", NAME)
                                .with_context("reason", "review_task_panicked"),
                            &request_id,
                        );
                    }
                }
            }
            match candidates(&gateway, &db, &in_flight).await {
                Ok(found) => {
                    failing = None;
                    for c in found {
                        let running = Running {
                            set: Arc::clone(&in_flight),
                            key: (c.mr.get(), c.head_sha.clone()), // clone: the key
                        };
                        let (gateway, db, source) = (
                            gateway.clone(), // clone: the task's own handle
                            db.clone(),      // clone: the task's own pool handle
                            Arc::clone(&source),
                        );
                        tasks.spawn(async move {
                            let _running = running;
                            let request_id = RequestId::generate();
                            // Failures were logged where they happened.
                            let _logged =
                                review_one(&gateway, &db, &*source, &c, &request_id).await;
                        });
                    }
                }
                Err(e) => {
                    if failing != Some(e.code()) {
                        failing = Some(e.code());
                        telemetry::log_failure(&e.with_context("task", NAME), &request_id);
                    }
                }
            }
            tokio::select! {
                () = tokio::time::sleep(PERIOD) => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        tasks.abort_all();
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
#[path = "ai_review_tests.rs"]
mod tests;
