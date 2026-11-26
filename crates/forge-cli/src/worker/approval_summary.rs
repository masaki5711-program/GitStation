//! The approval summaries (M5 L5-8, plan 19 §3.4, ADR-0039 Amendment
//! L5-8): every [`PERIOD`], for each deployment waiting for an approval
//! inside its window (at most `forge_db::deploy::MAX_SUMMARY_CANDIDATES`),
//!
//! 1. the facts are built by code (`Deploy::approval_facts`) and prepared
//!    as the input (`summary_input`: `internal`, secrets redacted);
//! 2. unless these very facts were already asked about
//!    (`forge_deploy::summary::should_ask` on the request rows: never
//!    while queued, running or answered; after a failure again only after
//!    10 minutes, 3 times at most — and the same bound in memory for a
//!    failure that left no row), the gateway is asked (interactive tier,
//!    the approval prompt, no findings);
//! 3. the outcome is the event `deployment.approval_summarized` (ready,
//!    or unavailable with the LLM's code) for Kanflow's approval card.
//!
//! The approval never waits for any of this and reads none of it. Runs
//! only with the gateway (`llm.enabled`); without it the facts are still
//! served by the approval-summary route. The gateway logs its own
//! failures. One deployment's failure never stops the others: the pass
//! goes on and returns the first error, logged once per streak with its
//! code and `task=approval_summary`. A range git cannot read and an input
//! refused (`FG-LLM-0005`) are logged once per deployment and code / facts.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use forge_core::deploy::DeploymentStatus;
use forge_core::llm::{OutputPolicy, Purpose, SubjectKind};
use forge_core::{telemetry, ProjectId, RequestId};
use forge_db::deploy::{SummaryCandidate, SummaryOutcome};
use forge_db::Db;
use forge_deploy::summary::{log_range, should_ask, summary_input, MAX_ASKS, RETRY_AFTER_SECS};
use forge_deploy::Deploy;
use forge_errors::{ErrorCode, ForgeError};
use forge_llm::{Gateway, Outcome, Submission};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The task name in the logs.
pub(crate) const NAME: &str = "approval_summary";
/// How often it looks for waiting deployments.
pub(crate) const PERIOD: Duration = Duration::from_secs(30);
/// The memory is cleared beyond this many entries (a restart forgets too).
const MAX_REMEMBERED: usize = 10_000;

/// What one pass did (for the tests and the log).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PassCount {
    pub(crate) asked: usize,
    pub(crate) advised: usize,
    pub(crate) failed: usize,
}

/// What the task remembers between passes.
#[derive(Debug, Default)]
pub(crate) struct Memory {
    /// (deployment, facts hash) whose input was refused, logged once.
    refused: HashSet<(i64, String)>,
    /// (deployment, code) whose range git could not read, logged once.
    range_logged: HashSet<(i64, String)>,
    /// (deployment, input hash) → (attempts, last attempt) of failures
    /// that left no request row (the rows bound the others).
    attempts: HashMap<(i64, String), (i64, Instant)>,
}

impl Memory {
    fn trim(&mut self) {
        if self.refused.len() > MAX_REMEMBERED {
            self.refused.clear();
        }
        if self.range_logged.len() > MAX_REMEMBERED {
            self.range_logged.clear();
        }
        if self.attempts.len() > MAX_REMEMBERED {
            self.attempts.clear();
        }
    }

    /// Whether a failure without a row forbids asking now.
    fn held(&self, key: &(i64, String), now: Instant) -> bool {
        self.attempts.get(key).is_some_and(|(n, at)| {
            *n >= MAX_ASKS
                || now.duration_since(*at) < Duration::from_secs(RETRY_AFTER_SECS.unsigned_abs())
        })
    }
}

/// The policy of an approval summary: words only, no findings.
/// The same policy the eval bench measures (`forge_llm::prompts`).
pub(crate) fn policy() -> OutputPolicy {
    forge_llm::prompts::approval_summary_policy()
}

/// One pass (module docs).
pub(crate) async fn pass(
    gateway: &Gateway,
    db: &Db,
    deploy: &Deploy,
    request_id: &RequestId,
    memory: &mut Memory,
) -> Result<PassCount, ForgeError> {
    let mut count = PassCount::default();
    let candidates =
        forge_db::deploy::pending_for_summary(db, forge_db::deploy::MAX_SUMMARY_CANDIDATES).await?;
    let mut first_error = None;
    for c in candidates {
        if let Err(e) = one(gateway, db, deploy, request_id, &c, memory, &mut count).await {
            if first_error.is_none() {
                first_error = Some(e.with_context("deployment_id", c.deployment));
            }
        }
    }
    memory.trim();
    match first_error {
        Some(e) => Err(e),
        None => Ok(count),
    }
}

async fn one(
    gateway: &Gateway,
    db: &Db,
    deploy: &Deploy,
    request_id: &RequestId,
    c: &SummaryCandidate,
    memory: &mut Memory,
    count: &mut PassCount,
) -> Result<(), ForgeError> {
    let Some(d) = forge_db::deploy::get_deployment(db, c.project, c.deployment).await? else {
        return Ok(());
    };
    if d.status != DeploymentStatus::PendingApproval {
        return Ok(());
    }
    let read = deploy.approval_facts(&d).await?;
    if let Some(e) = &read.range_error {
        if memory.range_logged.insert((d.id, e.code().id().to_owned())) {
            log_range(e, request_id, &d);
        }
    }
    let facts = read.facts;
    let input = match summary_input(&facts) {
        Ok(input) => input,
        Err(e) => {
            // The facts cannot be sent (a secret that cannot be redacted):
            // the approver sees them without a summary. Logged once.
            if memory.refused.insert((d.id, facts_key(&facts))) {
                telemetry::log_failure(
                    &e.with_context("task", NAME)
                        .with_context("deployment_id", d.id)
                        .with_context("project_id", d.project_id),
                    request_id,
                );
            }
            return Ok(());
        }
    };
    let key = (d.id, input.sha256().to_owned());
    if memory.held(&key, Instant::now()) {
        return Ok(());
    }
    let before = forge_db::llm::asked_before(
        db,
        Purpose::ApprovalSummary,
        SubjectKind::Deployment,
        d.id,
        input.sha256(),
        RETRY_AFTER_SECS,
    )
    .await?;
    if !should_ask(&before) {
        return Ok(());
    }
    // The newest row before asking: a failure's row is this attempt's only
    // when it is a newer one.
    let earlier = forge_db::llm::last_request(
        db,
        Purpose::ApprovalSummary,
        SubjectKind::Deployment,
        d.id,
        &key.1,
    )
    .await?
    .map(|(id, _)| id);
    let project = ProjectId::new(d.project_id);
    count.asked += 1;
    let submitted = gateway
        .submit(Submission {
            purpose: Purpose::ApprovalSummary,
            priority: None,
            subject_kind: SubjectKind::Deployment,
            subject_id: Some(d.id),
            project: Some(project),
            requested_by: None,
            head_sha: Some(d.sha.clone()), // clone: the advice binds to it
            prompt: forge_llm::prompts::approval_summary(),
            input,
            policy: policy(),
            request_id: request_id.clone(), // clone: the submission owns it
        })
        .await;
    let (llm_request, advice, error_code) = match submitted {
        Ok(Outcome::Advised(a)) => {
            count.advised += 1;
            memory.attempts.remove(&key);
            (Some(a.llm_request_id), Some(a.advice_id), None)
        }
        // Nothing was queued: asked again on a later pass.
        Ok(Outcome::Paused) => return Ok(()),
        Err(e) => {
            // The gateway logged it; the event says the summary is
            // unavailable (the card shows the facts alone).
            count.failed += 1;
            let last = forge_db::llm::last_request(
                db,
                Purpose::ApprovalSummary,
                SubjectKind::Deployment,
                d.id,
                &key.1,
            )
            .await?;
            match last {
                // Its row bounds the next attempts (`should_ask`).
                Some((id, code)) if Some(id) != earlier => (
                    Some(id),
                    None,
                    Some(code.unwrap_or_else(|| e.code().id().to_owned())),
                ),
                // No row of this attempt (unreachable before queueing, the
                // model stopped): bounded in memory.
                Some(_) | None => {
                    let entry = memory
                        .attempts
                        .entry(key.clone()) // clone: the map owns its key
                        .or_insert((0, Instant::now()));
                    entry.0 += 1;
                    entry.1 = Instant::now();
                    (None, None, Some(e.code().id().to_owned()))
                }
            }
        }
    };
    let attempt_minute = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs() / 60).unwrap_or(i64::MAX))
        .unwrap_or(0);
    forge_db::deploy::record_approval_summarized(
        db,
        request_id,
        &SummaryOutcome {
            project,
            deployment: d.id,
            tier: d.tier,
            sha: d.sha.clone(), // clone: the event owns it
            advice,
            llm_request,
            attempt_minute,
            error_code,
        },
    )
    .await?;
    tracing::info!(
        target: "forge_cli",
        event = "forge.deploy.approval_summarized",
        request_id = %request_id,
        deployment_id = d.id,
        project_id = d.project_id,
        llm_request_id = llm_request,
        advice_id = advice,
        "the approval summary of a waiting deployment was recorded"
    );
    Ok(())
}

/// The refused-input memory's key: the facts' text hash.
pub(crate) fn facts_key(facts: &forge_deploy::ApprovalFacts) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(forge_deploy::summary::render_facts(facts).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Run [`pass`] every [`PERIOD`] until `stop`.
pub(crate) fn spawn(
    gateway: Gateway,
    db: Db,
    deploy: Deploy,
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
            match pass(&gateway, &db, &deploy, &request_id, &mut memory).await {
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
#[path = "approval_summary_tests.rs"]
mod tests;
