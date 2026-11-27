//! The agent builder's recommendation reasons (plan 16 A11, ADR-0032
//! Amendment A11, ADR-0039 Amendment A11): every [`PERIOD`], for each
//! project whose recommendation was read within [`ASK_WINDOW_SECS`] (at
//! most [`MAX_PER_PASS`], `forge.agent_recommend_asks`) and that has no
//! answer for its tip yet,
//!
//! 1. a paused gateway is recorded as `paused` (nothing is read or asked);
//! 2. the rules-only recommendation, its tip and the stable catalog are
//!    read ([`RulesSource`]: forge-marketplace's `recommend_at`);
//! 3. the tip's specification documents are read ([`open_specs`]): only
//!    those that `forge_checks::confidential` and the document's own mark
//!    both leave open — a confidential document is **never** sent, and a
//!    tip whose documents cannot be classified sends nothing (fail
//!    closed, `refused`);
//! 4. the input is built by code (`forge_core::llm::agent_recommend`,
//!    `internal`, secrets redacted) and, unless this input is being asked
//!    about, failed within [`RETRY_AFTER_SECS`] or was asked [`MAX_ASKS`]
//!    times (the same bound in memory for a failure that left no row), the
//!    gateway is asked (interactive: a person has the builder open);
//! 5. the answer is recorded: `ready` with the advice, `paused`,
//!    `unavailable` with the gateway's FG code, or `refused`.
//!
//! The reasons decide nothing: the route merges them into the rules'
//! selection without changing it (`agent_recommend::merge`). Every
//! non-answer is logged once per project, tip and cause as
//! `FG-AGENT-0011` (warn) with `reason` and `llm_error`; the gateway logs
//! its own failures. One project's failure never stops the others: the
//! pass goes on and returns the first error, logged once per streak with
//! its code and `task=agent_recommend`. Runs only with the gateway.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use forge_checks::confidential::{confidential_at, ConfidentialLimits};
use forge_checks::spec_index::{index_commit, IndexLimits, Tags};
use forge_core::llm::agent_recommend::{
    policy, recommend_input, AskState, RulePick, SpecText, MAX_SPEC_BYTES, MAX_SPEC_DOCS,
};
use forge_core::llm::{Purpose, SubjectKind};
use forge_core::{telemetry, ProjectAccessLoader as _, RepoId, RepositoryState, RequestId};
use forge_db::llm::agent_recommend::{recent_asks, record_answer, Answer, RecentAsk};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, ListedBlob, RepoPath};
use forge_llm::{Gateway, Outcome, Submission};
use forge_marketplace::{Marketplace, RecommendedAt};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The task name in the logs.
pub(crate) const NAME: &str = "agent_recommend";
/// How often it looks for asks (a person has the builder open).
pub(crate) const PERIOD: Duration = Duration::from_secs(20);
/// Asks read within this long are answered.
pub(crate) const ASK_WINDOW_SECS: i64 = 60 * 60;
/// The most asks one pass looks at.
pub(crate) const MAX_PER_PASS: i64 = 20;
/// A failed ask (or a refused one) is tried again after this long.
pub(crate) const RETRY_AFTER_SECS: i64 = 10 * 60;
/// The most asks of one input.
pub(crate) const MAX_ASKS: i64 = 3;
/// The most files of the specification directories listed.
const MAX_LISTED: usize = 4_000;
/// The memory is cleared beyond this many entries (a restart forgets too).
const MAX_REMEMBERED: usize = 10_000;

/// The boxed future of [`RulesSource::recommend_at`].
pub(crate) type RulesFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RecommendedAt, ForgeError>> + Send + 'a>>;

/// Where the rules-only recommendation is read (the marketplace in the
/// worker; scripted in tests).
pub(crate) trait RulesSource: Send + Sync {
    fn recommend_at(&self, repo: RepoId) -> RulesFuture<'_>;
}

impl RulesSource for Marketplace {
    fn recommend_at(&self, repo: RepoId) -> RulesFuture<'_> {
        Box::pin(Marketplace::recommend_at(self, repo))
    }
}

/// What one pass did (for the tests and the log).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PassCount {
    pub(crate) asked: usize,
    pub(crate) answered: usize,
    pub(crate) not_answered: usize,
    pub(crate) skipped: usize,
}

/// What the task remembers between passes.
#[derive(Debug, Default)]
pub(crate) struct Memory {
    /// (project, tip, cause) logged once.
    logged: HashSet<(i64, String, String)>,
    /// (project, input hash) → (attempts, last attempt) of failures that
    /// left no request row (the rows bound the others).
    attempts: HashMap<(i64, String), (i64, Instant)>,
}

impl Memory {
    fn trim(&mut self) {
        if self.logged.len() > MAX_REMEMBERED {
            self.logged.clear();
        }
        if self.attempts.len() > MAX_REMEMBERED {
            self.attempts.clear();
        }
    }

    fn held(&self, key: &(i64, String), now: Instant) -> bool {
        self.attempts.get(key).is_some_and(|(n, at)| {
            *n >= MAX_ASKS
                || now.duration_since(*at) < Duration::from_secs(RETRY_AFTER_SECS.unsigned_abs())
        })
    }
}

/// The handles one pass uses.
pub(crate) struct Deps<'a> {
    pub(crate) gateway: &'a Gateway,
    pub(crate) db: &'a Db,
    pub(crate) rules: &'a dyn RulesSource,
    pub(crate) browser: &'a Browser,
}

/// The open specification documents of `tip` (module docs, 3): at most
/// [`MAX_SPEC_DOCS`], in path order, each at most [`MAX_SPEC_BYTES`] and
/// UTF-8; and how many documents were withheld as confidential. An error
/// (git, the classification's bound): nothing may be sent.
pub(crate) async fn open_specs(
    browser: &Browser,
    repo: RepoId,
    tip: &str,
) -> Result<(Vec<SpecText>, usize), ForgeError> {
    let indexed = index_commit(browser, repo, tip, &IndexLimits::default(), Tags::Skip).await?;
    let marks = confidential_at(browser, repo, tip, &ConfidentialLimits::default()).await?;
    let mut withheld = 0usize;
    let mut open: Vec<&str> = Vec::new();
    for d in &indexed.index.documents {
        if d.doc.confidential() || marks.contains(&d.path) {
            withheld += 1;
        } else {
            open.push(d.path.as_str());
        }
    }
    open.sort_unstable();
    let dirs: Vec<RepoPath> = indexed
        .dirs
        .iter()
        .filter_map(|d| RepoPath::parse(d).ok())
        .collect();
    let listing = browser.blobs_under(repo, tip, &dirs, MAX_LISTED).await?;
    let wanted: Vec<ListedBlob> = open
        .iter()
        .filter_map(|p| listing.blobs.iter().find(|b| b.path == *p))
        .filter(|b| b.size <= MAX_SPEC_BYTES)
        .take(MAX_SPEC_DOCS)
        .cloned() // clone: read below
        .collect();
    let contents = browser.read_blobs(repo, &wanted).await?;
    let texts = wanted
        .iter()
        .zip(contents)
        .filter_map(|(b, bytes)| {
            String::from_utf8(bytes).ok().map(|text| SpecText {
                path: b.path.clone(), // clone: the text owns its path
                text,
            })
        })
        .collect();
    Ok((texts, withheld))
}

fn picks(found: &RecommendedAt) -> Vec<RulePick> {
    found
        .recommendation
        .parts
        .iter()
        .map(|p| RulePick {
            slug: p.slug.clone(),       // clone: the pure layer owns its picks
            kind: p.kind.clone(),       // clone: as above
            version: p.version.clone(), // clone: as above
            reason: p.reason.clone(),   // clone: as above
        })
        .collect()
}

/// One pass (module docs).
pub(crate) async fn pass(
    deps: &Deps<'_>,
    request_id: &RequestId,
    memory: &mut Memory,
) -> Result<PassCount, ForgeError> {
    let mut count = PassCount::default();
    let found = recent_asks(deps.db, ASK_WINDOW_SECS, RETRY_AFTER_SECS, MAX_PER_PASS).await?;
    let mut first_error = None;
    for a in found {
        if let Err(e) = one(deps, request_id, &a, memory, &mut count).await {
            if first_error.is_none() {
                first_error = Some(e.with_context("project_id", a.project.get()));
            }
        }
    }
    memory.trim();
    match first_error {
        Some(e) => Err(e),
        None => Ok(count),
    }
}

/// Whether `a` needs an answer now.
fn wanted(a: &RecentAsk) -> bool {
    match a.ask.state {
        AskState::Ready => a.ask.advice_id.is_none(),
        AskState::Pending | AskState::Paused => true,
        AskState::Unavailable | AskState::Refused => a.updated_secs >= RETRY_AFTER_SECS,
    }
}

/// Why no reasons: logged once per project, tip and cause.
struct NoAnswer<'a> {
    state: AskState,
    tip: Option<&'a str>,
    reason: &'static str,
    cause: Option<ErrorCode>,
}

fn paused(tip: Option<&str>) -> NoAnswer<'_> {
    NoAnswer {
        state: AskState::Paused,
        tip,
        reason: "paused",
        cause: None,
    }
}

async fn not_answered(
    deps: &Deps<'_>,
    request_id: &RequestId,
    a: &RecentAsk,
    memory: &mut Memory,
    count: &mut PassCount,
    n: NoAnswer<'_>,
) -> Result<(), ForgeError> {
    count.not_answered += 1;
    record_answer(
        deps.db,
        a.project,
        &Answer {
            state: n.state,
            head_sha: n.tip,
            error_code: n.cause,
            advice_id: None,
        },
    )
    .await?;
    let cause = n.cause.map_or("", |c| c.id());
    let key = (
        a.project.get(),
        n.tip.unwrap_or("").to_owned(),
        format!("{}:{cause}", n.reason),
    );
    if memory.logged.insert(key) {
        let mut e = ForgeError::new(ErrorCode::RecommendDegraded)
            .with_context("task", NAME)
            .with_context("project_id", a.project.get())
            .with_context("reason", n.reason)
            .with_context("tip", n.tip.unwrap_or("-").to_owned());
        if let Some(c) = n.cause {
            e = e.with_context("llm_error", c.id());
        }
        telemetry::log_failure(&e, request_id);
    }
    Ok(())
}

async fn one(
    deps: &Deps<'_>,
    request_id: &RequestId,
    a: &RecentAsk,
    memory: &mut Memory,
    count: &mut PassCount,
) -> Result<(), ForgeError> {
    if !wanted(a) {
        return Ok(());
    }
    if deps.gateway.is_paused() {
        let tip = a.ask.head_sha.as_deref();
        return not_answered(deps, request_id, a, memory, count, paused(tip)).await;
    }
    let Some(record) = deps.db.load_project(a.project, None).await? else {
        count.skipped += 1;
        return Ok(());
    };
    if record.repository.state != RepositoryState::Ready {
        count.skipped += 1;
        return Ok(());
    }
    let repo = record.repository.id;
    let found = deps.rules.recommend_at(repo).await?;
    let Some(tip) = found.tip.as_ref().map(|t| t.as_str().to_owned()) else {
        let n = NoAnswer {
            state: AskState::Refused,
            tip: None,
            reason: "no_spec",
            cause: None,
        };
        return not_answered(deps, request_id, a, memory, count, n).await;
    };
    let tip = tip.as_str();
    let specs = match open_specs(deps.browser, repo, tip).await {
        Ok((specs, _withheld)) => specs,
        Err(e) => {
            // Fail closed: what cannot be classified is never sent.
            let n = NoAnswer {
                state: AskState::Refused,
                tip: Some(tip),
                reason: "confidential_unreadable",
                cause: Some(e.code()),
            };
            return not_answered(deps, request_id, a, memory, count, n).await;
        }
    };
    if specs.is_empty() {
        let n = NoAnswer {
            state: AskState::Refused,
            tip: Some(tip),
            reason: "no_spec",
            cause: None,
        };
        return not_answered(deps, request_id, a, memory, count, n).await;
    }
    let rules = picks(&found);
    let input = match recommend_input(&rules, &found.catalog, &specs) {
        Ok(i) => i,
        Err(e) => {
            let n = NoAnswer {
                state: AskState::Refused,
                tip: Some(tip),
                reason: "input_refused",
                cause: Some(e.code()),
            };
            return not_answered(deps, request_id, a, memory, count, n).await;
        }
    };
    let subject = a.project.get();
    let key = (subject, input.sha256().to_owned());
    if memory.held(&key, Instant::now()) {
        return Ok(());
    }
    let before = forge_db::llm::asked_before(
        deps.db,
        Purpose::AgentRecommend,
        SubjectKind::Project,
        subject,
        input.sha256(),
        RETRY_AFTER_SECS,
    )
    .await?;
    if before.live_or_answered > 0 {
        // Answered already (the answer was not recorded: a stop between
        // the two writes), or being asked: recorded when it is there.
        if let Some(label) = forge_db::llm::latest_advice(
            deps.db,
            SubjectKind::Project,
            subject,
            Purpose::AgentRecommend,
        )
        .await?
        .filter(|l| {
            l.head_sha.as_deref() == Some(tip) && l.input_sha256.as_deref() == Some(input.sha256())
        }) {
            count.answered += 1;
            record_answer(
                deps.db,
                a.project,
                &Answer {
                    state: AskState::Ready,
                    head_sha: Some(tip),
                    error_code: None,
                    advice_id: Some(label.id),
                },
            )
            .await?;
            return Ok(());
        }
        // Being asked; or answered once but the advice was swept (an ask
        // recorded `ready` whose advice is gone): asked again, within the
        // bound below.
        let swept = a.ask.state == AskState::Ready && a.ask.advice_id.is_none();
        if !swept {
            return Ok(());
        }
    }
    if before.failed_recently > 0 || before.total >= MAX_ASKS {
        return Ok(());
    }
    let earlier = forge_db::llm::last_request(
        deps.db,
        Purpose::AgentRecommend,
        SubjectKind::Project,
        subject,
        &key.1,
    )
    .await?
    .map(|(id, _)| id);
    count.asked += 1;
    let submitted = deps
        .gateway
        .submit(Submission {
            purpose: Purpose::AgentRecommend,
            priority: None,
            subject_kind: SubjectKind::Project,
            subject_id: Some(subject),
            project: Some(a.project),
            requested_by: None,
            head_sha: Some(tip.to_owned()),
            prompt: forge_llm::prompts::agent_recommend(),
            input,
            policy: policy(),
            // No deep-review rule applies to a recommendation.
            deep_review: None,
            request_id: request_id.clone(), // clone: the submission owns it
        })
        .await;
    match submitted {
        Ok(Outcome::Advised(advised)) => {
            count.answered += 1;
            record_answer(
                deps.db,
                a.project,
                &Answer {
                    state: AskState::Ready,
                    head_sha: Some(tip),
                    error_code: None,
                    advice_id: Some(advised.advice_id),
                },
            )
            .await?;
            tracing::info!(
                target: "forge_cli",
                event = "forge.llm.agent_recommend_answered",
                request_id = %request_id,
                task = NAME,
                project_id = subject,
                tip,
                llm_request_id = advised.llm_request_id,
                advice_id = advised.advice_id,
                findings = advised.advice.findings.len(),
                degraded = advised.advice.degraded,
                "the local LLM's reasons for the agent recommendation were recorded"
            );
            Ok(())
        }
        Ok(Outcome::Paused) => {
            not_answered(deps, request_id, a, memory, count, paused(Some(tip))).await
        }
        Err(e) => {
            // The gateway logged it. A failure that left no row is
            // bounded in memory.
            let last = forge_db::llm::last_request(
                deps.db,
                Purpose::AgentRecommend,
                SubjectKind::Project,
                subject,
                &key.1,
            )
            .await?
            .map(|(id, _)| id);
            if last.is_none() || last == earlier {
                let entry = memory.attempts.entry(key).or_insert((0, Instant::now()));
                entry.0 += 1;
                entry.1 = Instant::now();
            }
            let n = NoAnswer {
                state: AskState::Unavailable,
                tip: Some(tip),
                reason: "llm_unavailable",
                cause: Some(e.code()),
            };
            not_answered(deps, request_id, a, memory, count, n).await
        }
    }
}

/// Start the task (module docs).
pub(crate) fn spawn(
    gateway: Gateway,
    db: Db,
    market: Marketplace,
    browser: Browser,
    mut stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut failing: Option<ErrorCode> = None;
        let mut memory = Memory::default();
        loop {
            if *stop.borrow() {
                return;
            }
            let request_id = RequestId::generate();
            let deps = Deps {
                gateway: &gateway,
                db: &db,
                rules: &market,
                browser: &browser,
            };
            match pass(&deps, &request_id, &mut memory).await {
                Ok(_) => failing = None,
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
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
#[path = "agent_recommend_tests.rs"]
mod tests;
