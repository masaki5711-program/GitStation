//! The approval summary of a deployment (M5 L5-8, plan 19 §3.4, W14-11;
//! ADR-0039 Amendment L5-8). **Code makes the facts, the local LLM only
//! rewords them, and nothing here decides anything.**
//!
//! - [`ApprovalFacts`] — built by [`Deploy::approval_facts`] from git and
//!   the database, never from a model: the merge requests since the
//!   environment's last succeeded deployment, the migrations by name,
//!   protected paths, CI / deploy configuration and permission code that
//!   changed (the path rules of `forge_core::risk`), the `forge/risk`
//!   result, the failed and `error` checks of the commit, and the
//!   environment's freeze windows. A range git cannot read is said so
//!   ([`RangeState::Unavailable`] with its code, logged once), never
//!   shown as "nothing changed".
//! - [`summary_input`] — the facts as the LLM's input (`internal`: titles,
//!   paths and check names; secrets redacted by `forge_core::llm::prepare`).
//!   Its SHA-256 binds an advice to the facts it summarized.
//! - [`Deploy::approval_summary`] (`DeployRead`) — the facts **in full**
//!   and, apart from them, the newest summary: shown, withheld from an
//!   agent when confidential, or none ("AI の助言は今ありません"). A
//!   summary is **stale** ([`StaleReason`]) when it was made for another
//!   commit, a newer commit was requested for the environment, or the
//!   facts are no longer the ones it summarized.
//!
//! The approval (`Deploy::approve`) reads none of this: approving works
//! with no summary, with the LLM down, and whatever a summary says
//! (`approve_works_without_summary`, `summary_never_changes_approval_state`).

use forge_auth::{
    advice_reader, authorize_advice, Action, AdviceReader, Authorized, ProjectCtx, ResourceCtx as _,
};
use forge_core::deploy::DeployTier;
use forge_core::llm::{
    prepare, InputClass, InputLimits, InputPart, PreparedInput, Purpose, SubjectKind,
};
use forge_core::risk::{path_findings, ChangedFile};
use forge_core::{telemetry, ObjectId, ProjectId, RequestId};
use forge_db::llm::{AdviceLabel, AdviceView, ReaderKind};
use forge_errors::{ErrorCode, ForgeError};

use crate::commits::RangeChanges;
use crate::{require, Deploy, StoredDeployment};

/// The prompt id of the approval summary (`forge-llm/prompts`).
pub const PROMPT_ID: &str = "approval_summary";
/// The most entries one fact list holds (more: `lists_truncated`).
pub const MAX_LISTED: usize = 200;
/// The longest a merge request title is in the facts (characters).
pub const MAX_TITLE_CHARS: usize = 200;
/// The longest summary kept (characters).
pub const MAX_SUMMARY_CHARS: usize = 600;
/// What the screens say when there is no summary (plan 19 §0.3 5).
pub const NO_SUMMARY_JA: &str = "AI の助言は今ありません";

/// Whether the range since the last succeeded deployment was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeState {
    /// The environment never deployed with success: no range, every list
    /// of changes is empty because there is nothing to compare with.
    FirstDeployment,
    Read,
    /// git could not read it (`error_code`; logged once): the lists of
    /// changes are unknown, not empty.
    Unavailable {
        error_code: String,
    },
}

impl RangeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FirstDeployment => "first_deployment",
            Self::Read => "read",
            Self::Unavailable { .. } => "unavailable",
        }
    }
}

/// A merge request the deployment brings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRequestFact {
    pub iid: i64,
    pub title: String,
    /// The `forge/risk` conclusion at the head it merged (`None`: none).
    pub risk: Option<String>,
}

/// A path a rule names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFact {
    pub path: String,
    /// The `forge_core::risk` rule (or `repository_rules`).
    pub rule: String,
}

/// A completed check that did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckFact {
    pub name: String,
    pub conclusion: String,
}

/// The facts of one deployment's approval (module docs). Built by code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalFacts {
    pub deployment: i64,
    pub tier: DeployTier,
    pub sha: String,
    /// The environment's last succeeded deployment (the range's base).
    pub base_sha: Option<String>,
    pub range: RangeState,
    /// Commits in the range (`commits_truncated`: more than were read).
    pub commits: usize,
    pub commits_truncated: bool,
    pub merge_requests: Vec<MergeRequestFact>,
    pub migrations: Vec<String>,
    pub protected_paths: Vec<PathFact>,
    pub ci_config: Vec<String>,
    pub permissions: Vec<String>,
    /// git's diff or a list here was cut: the lists may be incomplete.
    pub lists_truncated: bool,
    /// The `forge/risk` conclusion of the commit itself (`None`: none).
    pub risk: Option<String>,
    pub failed_checks: Vec<CheckFact>,
    /// `<start>/<end>` (UTC) of the environment's freeze windows.
    pub freeze_windows: Vec<String>,
}

/// The rules of `forge_core::risk` that name a protected path.
const PROTECTED_RULES: &[&str] = &[
    "agent_config",
    "secret_file",
    "data_lifecycle_job",
    "special_entry",
    "submodule_change",
];

/// Paths that change the repository's own rules (review, merge, CI).
fn is_repository_rules(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.starts_with(".forge/") || name == "CODEOWNERS" || name == ".gitattributes"
}

fn push_capped<T: PartialEq>(list: &mut Vec<T>, item: T, cut: &mut bool) {
    if list.contains(&item) {
        return;
    }
    if list.len() >= MAX_LISTED {
        *cut = true;
        return;
    }
    list.push(item);
}

/// The lists of changes of `files` (migrations, protected paths, CI /
/// deploy configuration, permission code) and whether one was cut.
pub fn classify(
    files: &[ChangedFile],
) -> (Vec<String>, Vec<PathFact>, Vec<String>, Vec<String>, bool) {
    let (mut migrations, mut protected, mut ci, mut perms) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut cut = false;
    for file in files {
        for f in path_findings(file) {
            let path = f.path.clone().unwrap_or_else(|| file.path.clone()); // clone: the fact owns it
            match f.rule {
                "new_migration" | "applied_migration_changed" => {
                    push_capped(&mut migrations, path, &mut cut)
                }
                "ci_deploy_config" => push_capped(&mut ci, path, &mut cut),
                "auth_change" => push_capped(&mut perms, path, &mut cut),
                rule if PROTECTED_RULES.contains(&rule) => push_capped(
                    &mut protected,
                    PathFact {
                        path,
                        rule: rule.to_owned(),
                    },
                    &mut cut,
                ),
                _ => {}
            }
        }
        // forge's own pipeline file (the risk rules know other CI files).
        if file.path == ".forge-ci.yml" || file.old_path.as_deref() == Some(".forge-ci.yml") {
            push_capped(&mut ci, file.path.clone(), &mut cut); // clone: the fact owns it
        }
        if is_repository_rules(&file.path) {
            push_capped(
                &mut protected,
                PathFact {
                    path: file.path.clone(), // clone: as above
                    rule: "repository_rules".to_owned(),
                },
                &mut cut,
            );
        }
    }
    (migrations, protected, ci, perms, cut)
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn title(t: &str) -> String {
    t.chars()
        .take(MAX_TITLE_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The facts as one text, every list in full (deterministic: the same
/// facts give the same text, so its hash binds an advice to them). Never
/// time-dependent (whether a window holds *now* is not in it).
pub fn render_facts(f: &ApprovalFacts) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    line(format!("environment: {}", f.tier.as_str()));
    line(format!("commit: {}", f.sha));
    line(format!(
        "previous deployment: {}",
        f.base_sha.as_deref().map_or("none", short)
    ));
    line(format!("range: {}", f.range.as_str()));
    line(format!(
        "commits: {}{}",
        f.commits,
        if f.commits_truncated { " (more)" } else { "" }
    ));
    line(format!("merge requests: {}", f.merge_requests.len()));
    for m in &f.merge_requests {
        line(format!(
            "- !{} {} (forge/risk: {})",
            m.iid,
            title(&m.title),
            m.risk.as_deref().unwrap_or("none")
        ));
    }
    line(format!("migrations: {}", f.migrations.len()));
    for p in &f.migrations {
        line(format!("- {p}"));
    }
    line(format!("protected paths: {}", f.protected_paths.len()));
    for p in &f.protected_paths {
        line(format!("- {} ({})", p.path, p.rule));
    }
    line(format!("ci/deploy configuration: {}", f.ci_config.len()));
    for p in &f.ci_config {
        line(format!("- {p}"));
    }
    line(format!("permission code: {}", f.permissions.len()));
    for p in &f.permissions {
        line(format!("- {p}"));
    }
    if f.lists_truncated {
        line("lists: truncated".to_owned());
    }
    line(format!(
        "forge/risk of the commit: {}",
        f.risk.as_deref().unwrap_or("none")
    ));
    line(format!("failed checks: {}", f.failed_checks.len()));
    for c in &f.failed_checks {
        line(format!("- {} ({})", c.name, c.conclusion));
    }
    line(format!("freeze windows: {}", f.freeze_windows.len()));
    for w in &f.freeze_windows {
        line(format!("- {w}"));
    }
    out
}

/// The facts as the LLM's input (`internal`). `FG-LLM-0005` when it
/// cannot be sent (a secret that cannot be redacted, too large).
pub fn summary_input(f: &ApprovalFacts) -> Result<PreparedInput, ForgeError> {
    prepare(
        vec![InputPart::new(
            "deployment_approval_facts",
            InputClass::Internal,
            render_facts(f),
        )],
        &InputLimits::default(),
    )
}

/// Why a summary no longer describes the approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// It was made for another commit.
    ShaChanged,
    /// A newer commit was requested for the same environment.
    NewerCommit,
    /// The facts it summarized are not the facts now (a check finished,
    /// the environment deployed something else meanwhile).
    FactsChanged,
    /// What it summarized is not known any more (its request was swept,
    /// or the facts cannot be prepared now).
    FactsUnknown,
}

impl StaleReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ShaChanged => "sha_changed",
            Self::NewerCommit => "newer_commit",
            Self::FactsChanged => "facts_changed",
            Self::FactsUnknown => "facts_unknown",
        }
    }
}

/// Whether the summary `label` still describes deployment `sha` with the
/// facts whose input hash is `facts_sha256` (`None`: not preparable);
/// `newer`: the commit of a later request of the same environment.
pub fn staleness(
    label: &AdviceLabel,
    sha: &str,
    facts_sha256: Option<&str>,
    newer: Option<&str>,
) -> Option<StaleReason> {
    if label.head_sha.as_deref() != Some(sha) {
        return Some(StaleReason::ShaChanged);
    }
    if newer.is_some_and(|n| n != sha) {
        return Some(StaleReason::NewerCommit);
    }
    match (label.input_sha256.as_deref(), facts_sha256) {
        (Some(then), Some(now)) if then == now => None,
        (Some(_), Some(_)) => Some(StaleReason::FactsChanged),
        _ => Some(StaleReason::FactsUnknown),
    }
}

/// Whether the worker asks the LLM for these facts now: never while a
/// request of the same facts is queued, running or answered; after a
/// failure, again only once 10 minutes passed, at most 3 times in all.
pub fn should_ask(before: &forge_db::llm::AskedBefore) -> bool {
    before.live_or_answered == 0 && before.failed_recently == 0 && before.total < MAX_ASKS
}

/// The most requests one set of facts gets (failures retried).
pub const MAX_ASKS: i64 = 3;
/// How long after a failure the same facts are asked again (seconds).
pub const RETRY_AFTER_SECS: i64 = 10 * 60;

/// The summary beside the facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryView {
    /// No advice (the LLM is off, down, or has not answered yet).
    None,
    Shown {
        label: AdviceLabel,
        text: String,
        degraded: bool,
        input_class: InputClass,
        stale: Option<StaleReason>,
    },
    /// Confidential, read by an agent: that it exists, never its words.
    Withheld {
        label: AdviceLabel,
        input_class: InputClass,
        stale: Option<StaleReason>,
    },
}

/// What [`Deploy::approval_facts`] read.
#[derive(Debug)]
pub struct FactsRead {
    pub facts: ApprovalFacts,
    /// A freeze window of the environment holds now.
    pub frozen_now: bool,
    /// Why the range is [`RangeState::Unavailable`] (to be logged once).
    pub range_error: Option<ForgeError>,
}

/// What an agent is not told of the facts: the names of files the path
/// rules call secret (`secret_file`); the rule and the count stay.
pub const WITHHELD_PATH: &str = "[path withheld]";

/// Replace the secret-file path names of `facts` for an agent reader.
pub fn withhold_secret_paths(facts: &mut ApprovalFacts) {
    for p in &mut facts.protected_paths {
        if p.rule == "secret_file" {
            p.path = WITHHELD_PATH.to_owned();
        }
    }
}

/// What `GET …/deployments/{deployment_id}/approval-summary` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalSummary {
    pub deployment: StoredDeployment,
    /// Always in full, whatever the summary says or leaves out.
    pub facts: ApprovalFacts,
    /// A freeze window of the environment holds now.
    pub frozen_now: bool,
    pub summary: SummaryView,
}

impl Deploy {
    /// The facts of deployment `d` (module docs). A range git cannot read
    /// is [`RangeState::Unavailable`] and its error is returned in
    /// [`FactsRead::range_error`] for the caller to log once (the route per
    /// request, the worker once per deployment and code); a database
    /// failure is the caller's error.
    pub async fn approval_facts(&self, d: &StoredDeployment) -> Result<FactsRead, ForgeError> {
        let project = ProjectId::new(d.project_id);
        let base = forge_db::deploy::last_succeeded_sha(&self.db, project, d.tier).await?;
        let mut facts = ApprovalFacts {
            deployment: d.id,
            tier: d.tier,
            sha: d.sha.clone(),     // clone: the facts own it
            base_sha: base.clone(), // clone: as sha
            range: RangeState::FirstDeployment,
            commits: 0,
            commits_truncated: false,
            merge_requests: Vec::new(),
            migrations: Vec::new(),
            protected_paths: Vec::new(),
            ci_config: Vec::new(),
            permissions: Vec::new(),
            lists_truncated: false,
            risk: None,
            failed_checks: Vec::new(),
            freeze_windows: Vec::new(),
        };
        let mut range_error = None;
        let mut commits = vec![d.sha.clone()]; // clone: the list owns it
        let to = ObjectId::parse(&d.sha).map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "stored_sha_invalid")
        })?;
        if let Some(base) = base.as_deref().filter(|b| *b != d.sha) {
            let from = ObjectId::parse(base).map_err(|_| {
                ForgeError::new(ErrorCode::Internal).with_context("reason", "stored_sha_invalid")
            })?;
            match self.commits.range(project, &from, &to).await {
                Ok(Some(RangeChanges {
                    commits: oids,
                    commits_truncated,
                    files,
                    files_truncated,
                })) => {
                    facts.range = RangeState::Read;
                    facts.commits = oids.len();
                    facts.commits_truncated = commits_truncated;
                    let (m, p, c, a, cut) = classify(&files);
                    facts.migrations = m;
                    facts.protected_paths = p;
                    facts.ci_config = c;
                    facts.permissions = a;
                    facts.lists_truncated = cut || files_truncated;
                    commits.extend(oids.into_iter().filter(|o| *o != d.sha));
                }
                Ok(None) => {
                    let e = ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                        .with_context("reason", "range_not_found");
                    facts.range = RangeState::Unavailable {
                        error_code: e.code().id().to_owned(),
                    };
                    range_error = Some(e);
                }
                Err(e) => {
                    facts.range = RangeState::Unavailable {
                        error_code: e.code().id().to_owned(),
                    };
                    range_error = Some(e);
                }
            }
        } else if base.as_deref() == Some(d.sha.as_str()) {
            // The environment runs this very commit: nothing in between.
            facts.range = RangeState::Read;
        }
        // More commits than the reads look at: say the lists may be short.
        facts.lists_truncated |= commits.len() > forge_db::deploy::MAX_SHAS;
        let merged = forge_db::deploy::merged_requests(&self.db, project, &commits).await?;
        let mut heads: Vec<String> = merged.iter().map(|m| m.head_sha.clone()).collect(); // clone: the query's list
        heads.push(d.sha.clone()); // clone: as above
        let checks = forge_db::deploy::completed_checks(&self.db, project, &heads).await?;
        let risk_at = |sha: &str| {
            checks
                .iter()
                .find(|c| c.head_sha == sha && c.name == "forge/risk")
                .map(|c| c.conclusion.clone()) // clone: the fact owns it
        };
        facts.merge_requests = merged
            .iter()
            .take(MAX_LISTED)
            .map(|m| MergeRequestFact {
                iid: m.iid,
                title: m.title.clone(), // clone: the fact owns it
                risk: risk_at(&m.head_sha),
            })
            .collect();
        facts.lists_truncated |= merged.len() > MAX_LISTED;
        facts.risk = risk_at(&d.sha);
        facts.failed_checks = checks
            .iter()
            // Every completed conclusion but a pass: failure, error,
            // action_required (and any added later).
            .filter(|c| {
                c.head_sha == d.sha && !matches!(c.conclusion.as_str(), "success" | "neutral")
            })
            .map(|c| CheckFact {
                name: c.name.clone(),             // clone: the fact owns it
                conclusion: c.conclusion.clone(), // clone: as name
            })
            .collect();
        let envs = forge_db::deploy::list_environments(&self.db, project).await?;
        let env = envs.iter().find(|e| e.tier == d.tier);
        facts.freeze_windows = env.map(|e| e.freeze_windows.clone()).unwrap_or_default(); // clone: the fact owns it
        let frozen_now = env.is_some_and(|e| e.frozen_now);
        Ok(FactsRead {
            facts,
            frozen_now,
            range_error,
        })
    }

    /// `GET …/deployments/{deployment_id}/approval-summary` (`DeployRead`):
    /// the facts in full and, apart from them, the newest summary (module
    /// docs). The summary's words are read on the advice's own subject
    /// (`forge_auth::authorize_advice`): a confidential one is withheld
    /// from an agent. `declared_agent`: `Forge-Context-Audience: agent`.
    pub async fn approval_summary(
        &self,
        request_id: &RequestId,
        proof: &Authorized<ProjectCtx>,
        deployment: i64,
        declared_agent: bool,
    ) -> Result<ApprovalSummary, ForgeError> {
        require(proof.action(), Action::DeployRead)?;
        let project = *proof.id();
        let d = forge_db::deploy::get_deployment(&self.db, project, deployment)
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::NotFound)
                    .with_context("resource", "deployment")
                    .with_context("deployment_id", deployment)
            })?;
        let FactsRead {
            mut facts,
            frozen_now,
            range_error,
        } = self.approval_facts(&d).await?;
        if let Some(e) = &range_error {
            log_range(e, request_id, &d);
        }
        // Hashed before any redaction for the reader: staleness compares
        // the facts the model saw.
        let now = summary_input(&facts).ok();
        let reader = advice_reader(proof.principal(), declared_agent);
        if reader == AdviceReader::Agent {
            withhold_secret_paths(&mut facts);
        }
        let label = forge_db::llm::latest_advice(
            &self.db,
            SubjectKind::Deployment,
            d.id,
            Purpose::ApprovalSummary,
        )
        .await?;
        let summary = match label {
            None => SummaryView::None,
            Some(label) => {
                let newer =
                    forge_db::deploy::newer_request(&self.db, project, d.tier, d.id, &d.sha)
                        .await?;
                let stale = staleness(
                    &label,
                    &d.sha,
                    now.as_ref().map(PreparedInput::sha256),
                    newer.as_deref(),
                );
                let resource = proof.ctx().resource();
                let viewed = forge_db::llm::advice_decided(&self.db, label.id, |subject| {
                    if subject.kind != SubjectKind::Deployment
                        || subject.id != Some(d.id)
                        || subject.project != Some(project)
                    {
                        return Err(ForgeError::new(ErrorCode::Internal)
                            .with_context("reason", "advice_subject_mismatch")
                            .with_context("advice_id", subject.advice_id));
                    }
                    authorize_advice(
                        proof.principal(),
                        Action::DeployRead,
                        &resource,
                        declared_agent,
                    )
                    .map(|r| match r {
                        AdviceReader::Person => ReaderKind::Person,
                        AdviceReader::Agent => ReaderKind::Agent,
                    })
                })
                .await?;
                match viewed {
                    // Swept between the two reads: no summary.
                    None => SummaryView::None,
                    Some(AdviceView::Shown(a)) => SummaryView::Shown {
                        label,
                        text: a.summary,
                        degraded: a.degraded,
                        input_class: a.input_class,
                        stale,
                    },
                    Some(AdviceView::Withheld { input_class, .. }) => SummaryView::Withheld {
                        label,
                        input_class,
                        stale,
                    },
                }
            }
        };
        Ok(ApprovalSummary {
            deployment: d,
            facts,
            frozen_now,
            summary,
        })
    }
}

/// Log a range git could not read (once per call; the callers dedupe).
pub fn log_range(e: &ForgeError, request_id: &RequestId, d: &StoredDeployment) {
    let mut c = ForgeError::new(e.code());
    for (k, v) in e.context() {
        c = c.with_context(k, v);
    }
    let c = c
        .with_context("event", "forge.deploy.approval_facts_range_unavailable")
        .with_context("deployment_id", d.id)
        .with_context("project_id", d.project_id);
    telemetry::log_failure(&c, request_id);
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
