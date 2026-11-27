//! The local LLM's first pass over a merge request (M5 L5-5, plan 19 §3.1,
//! ADR-0039 Amendment L5-5): what code decides around the model's words.
//!
//! - **The input** ([`assemble`]): the title and the description (as data,
//!   cut to [`MAX_TITLE_CHARS`] / [`MAX_DESCRIPTION_CHARS`]) and one `diff`
//!   part per changed file of `merge base..head`, each with its path.
//!   Lock files, generated files, binaries and patches git did not give
//!   are left out and named ([`Exclusion`]); the patches together stay
//!   under [`MAX_DIFF_CHARS`] (a file past it is left out as
//!   `over_budget`). A part about a confidential path (a marked document,
//!   `forge_checks::confidential`, read by the caller) is `confidential`,
//!   so the whole advice is (plan 19 §5): shown to people only.
//! - **The deep-review mark** ([`deep_review_reasons`]): code rules first —
//!   migrations, authentication / authorization code, CI and deploy
//!   configuration, agent configuration, secret-looking files, concurrency
//!   primitives in added lines, a large diff, a diff not fully reviewed.
//!   The model only adds its own sentence (`forge_llm::deep_review`).
//! - **The conclusion** ([`conclusion`]): advice. No finding (and an
//!   answer that was not degraded) → `success`; findings, or a degraded
//!   answer (a verdict withheld, findings dropped for their evidence) →
//!   `neutral`; `failure` only when the project's `llm.review` is
//!   `required` and a finding is critical or high. A failure of the LLM is
//!   `error` ([`failed_run`]), never a success. Nothing the model writes —
//!   an extra `"conclusion"` or `"approve"` key, an injected "LGTM" — can
//!   reach the conclusion: it reads only the checked findings and flags.
//! - **The check run** ([`advised_run`]): the summary is labelled as AI
//!   advice with the model and the prompt version. A confidential advice's
//!   words never go into the run (check runs are read by agent tokens):
//!   the run says it is withheld and the merge request page shows it to
//!   people.
//! - **The reader's view** ([`AiReviewView`], `Review::ai_review`): the
//!   project's mode, the run on the current head and the newest advice,
//!   whose words are read on its own subject (`forge_auth::
//!   authorize_advice`): a confidential advice is withheld from agents.

use forge_core::checks::{
    CheckAnnotationLevel, CheckConclusion, CheckRun, CheckStatus, AI_REVIEW_CHECK,
    MAX_CHECK_ANNOTATIONS, MAX_CHECK_SUMMARY_BYTES,
};
use forge_core::llm::{Advice, FindingSeverity, InputClass, InputPart, ReviewMode};
use forge_core::risk::{path_findings, ChangeStatus, ChangedFile};
use forge_core::CheckName;
use forge_db::checks::{CheckAnnotation, NewCheckRun};
use forge_db::llm::{AdviceLabel, StoredAdvice};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Diff, DiffFile, DiffStatus, PatchOmitted};

/// The prompt id of the first pass (`forge-llm/prompts/mr_review.v<N>.txt`).
pub const PROMPT_ID: &str = "mr_review";
/// The title as sent (characters).
pub const MAX_TITLE_CHARS: usize = 300;
/// The description as sent (characters).
pub const MAX_DESCRIPTION_CHARS: usize = 2_000;
/// One file's patch at most (characters); a longer one is `too_large`.
pub const MAX_FILE_CHARS: usize = 6_000;
/// Every patch together at most (characters): the model's context
/// (`llm.num_ctx` 8192 tokens) also holds the prompt and the answer.
pub const MAX_DIFF_CHARS: usize = 14_000;
/// Changed lines (added + removed) from which a diff is large.
pub const LARGE_DIFF_LINES: u64 = 400;
/// Changed files from which a diff is large.
pub const LARGE_DIFF_FILES: usize = 20;
/// The label every AI advice carries (plan 19 §4).
pub const ADVICE_LABEL: &str = "AI の助言（提案） / AI advice (suggestion)";
/// What a reader sees for a confidential advice read by an agent.
pub const WITHHELD_JA: &str = "機密の入力のため表示しません";
/// The longest annotation message (the table's CHECK, 4 KiB).
const MAX_ANNOTATION_BYTES: usize = 4_096;
/// The longest annotation path (the reporters' rule).
const MAX_ANNOTATION_PATH_BYTES: usize = 1_024;

/// Why a changed file is not sent to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exclusion {
    /// A dependency lock file (`Cargo.lock`, `package-lock.json`, …).
    LockFile,
    /// Generated or vendored (`*.gen.ts`, `*.min.js`, `vendor/`, …).
    Generated,
    /// git reports it binary, or its patch is not UTF-8 text.
    Binary,
    /// Its patch is longer than git's cap or [`MAX_FILE_CHARS`].
    TooLarge,
    /// The patches before it used up [`MAX_DIFF_CHARS`].
    OverBudget,
    /// A file that holds secrets by its name (`.env`, `.pgpass`, `.netrc`,
    /// a key or keystore): never sent, whatever the redaction would catch.
    SecretFile,
}

impl Exclusion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LockFile => "lock_file",
            Self::Generated => "generated",
            Self::Binary => "binary",
            Self::TooLarge => "too_large",
            Self::OverBudget => "over_budget",
            Self::SecretFile => "secret_file",
        }
    }

    /// Part of the change was not reviewed for a reason of size (not the
    /// file's kind): the deep-review mark says so.
    pub fn cut_by_size(self) -> bool {
        matches!(self, Self::TooLarge | Self::OverBudget)
    }
}

/// Lock files by file name.
const LOCK_FILES: &[&str] = &[
    "cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "go.sum",
    "poetry.lock",
    "pipfile.lock",
    "gemfile.lock",
    "composer.lock",
    "flake.lock",
    "packages.lock.json",
    "pubspec.lock",
    "mix.lock",
    "uv.lock",
];

/// Generated file name endings (lowercase).
const GENERATED_SUFFIXES: &[&str] = &[
    ".min.js", ".min.css", ".map", ".pb.go", "_pb2.py", ".pb.rs", ".snap", ".svg",
];

/// Directories whose files are generated or vendored.
const GENERATED_DIRS: &[&str] = &[
    "vendor",
    "node_modules",
    "dist",
    "target",
    "__snapshots__",
    "third_party",
];

/// Key file endings never sent (besides `forge_core::risk`'s secret files).
const KEY_SUFFIXES: &[&str] = &[".pem", ".key", ".crt", ".der", ".kdbx", ".gpg", ".asc"];

/// Whether `path` is left out by its kind (`None`: it is sent).
pub fn excluded_kind(path: &str) -> Option<Exclusion> {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if forge_core::risk::is_secret_file_path(path) || KEY_SUFFIXES.iter().any(|s| name.ends_with(s))
    {
        return Some(Exclusion::SecretFile);
    }
    if LOCK_FILES.contains(&name) || name.ends_with(".lock") {
        return Some(Exclusion::LockFile);
    }
    let generated_name = name.contains(".gen.")
        || name.contains(".generated.")
        || name.starts_with("generated_")
        || GENERATED_SUFFIXES.iter().any(|s| name.ends_with(s));
    let generated_dir = lower
        .split('/')
        .rev()
        .skip(1)
        .any(|d| GENERATED_DIRS.contains(&d));
    (generated_name || generated_dir).then_some(Exclusion::Generated)
}

/// Why code recommends a deeper review (plan 19 §3.1: the rules decide).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DeepReason {
    Migration,
    AuthCode,
    CiDeployConfig,
    AgentConfig,
    SecretFile,
    Concurrency,
    LargeDiff,
    NotFullyReviewed,
}

impl DeepReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Migration => "migration",
            Self::AuthCode => "auth_code",
            Self::CiDeployConfig => "ci_deploy_config",
            Self::AgentConfig => "agent_config",
            Self::SecretFile => "secret_file",
            Self::Concurrency => "concurrency",
            Self::LargeDiff => "large_diff",
            Self::NotFullyReviewed => "not_fully_reviewed",
        }
    }

    fn words(self) -> &'static str {
        match self {
            Self::Migration => "a database migration",
            Self::AuthCode => "authentication / authorization code",
            Self::CiDeployConfig => "CI or deploy configuration",
            Self::AgentConfig => "agent configuration (.claude, CLAUDE.md)",
            Self::SecretFile => "a secret-looking file",
            Self::Concurrency => "concurrency (locks, tasks, atomics, unsafe)",
            Self::LargeDiff => "a large change",
            Self::NotFullyReviewed => "part of the change was not sent to the AI",
        }
    }
}

/// The code rules' text for the advice (`None`: no rule fired).
pub fn deep_review_text(reasons: &[DeepReason]) -> Option<String> {
    if reasons.is_empty() {
        return None;
    }
    let words: Vec<&str> = reasons.iter().map(|r| r.words()).collect();
    Some(format!(
        "A deeper review (Claude or a person) is recommended by code rules: {}",
        words.join("; ")
    ))
}

/// Added-line tokens of concurrency (a deeper look, not a defect).
const CONCURRENCY_TOKENS: &[&str] = &[
    "Mutex",
    "RwLock",
    "tokio::spawn",
    "thread::spawn",
    "Atomic",
    "unsafe ",
    "Semaphore",
    "select!",
    "Condvar",
];

fn path_of(f: &DiffFile) -> &str {
    f.new_path
        .as_deref()
        .or(f.old_path.as_deref())
        .unwrap_or("")
}

fn mode(text: &str) -> u32 {
    u32::from_str_radix(text.trim(), 8).unwrap_or(0)
}

fn changed_file(f: &DiffFile) -> ChangedFile {
    let status = match f.status {
        DiffStatus::Added | DiffStatus::Copied => ChangeStatus::Added,
        DiffStatus::Modified => ChangeStatus::Modified,
        DiffStatus::Deleted => ChangeStatus::Deleted,
        DiffStatus::Renamed => ChangeStatus::Renamed,
        DiffStatus::TypeChanged => ChangeStatus::TypeChanged,
    };
    let renamed = if f.status == DiffStatus::Renamed {
        f.old_path.clone() // clone: the rules' own copy
    } else {
        None
    };
    ChangedFile {
        path: path_of(f).to_owned(),
        old_path: renamed,
        status,
        old_mode: mode(&f.old_mode),
        new_mode: mode(&f.new_mode),
        added: None,
        deleted: None,
    }
}

/// Lines added and removed in a patch.
fn changed_lines(patch: &str) -> u64 {
    let n = patch
        .lines()
        .filter(|l| {
            (l.starts_with('+') && !l.starts_with("+++"))
                || (l.starts_with('-') && !l.starts_with("---"))
        })
        .count();
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The deep-review reasons of a diff (module docs), sorted and unique.
pub fn deep_review_reasons(diff: &Diff, excluded: &[(String, Exclusion)]) -> Vec<DeepReason> {
    let mut out = Vec::new();
    let mut lines = 0u64;
    for f in &diff.files {
        for finding in path_findings(&changed_file(f)) {
            let reason = match finding.rule {
                "new_migration" | "applied_migration_changed" => Some(DeepReason::Migration),
                "auth_change" => Some(DeepReason::AuthCode),
                "ci_deploy_config" => Some(DeepReason::CiDeployConfig),
                "agent_config" => Some(DeepReason::AgentConfig),
                "secret_file" => Some(DeepReason::SecretFile),
                _ => None,
            };
            out.extend(reason);
        }
        if let Some(patch) = &f.patch {
            lines = lines.saturating_add(changed_lines(patch));
            let mut added = patch
                .lines()
                .filter(|l| l.starts_with('+') && !l.starts_with("+++"));
            if added.any(|l| CONCURRENCY_TOKENS.iter().any(|t| l.contains(t))) {
                out.push(DeepReason::Concurrency);
            }
        }
    }
    if lines >= LARGE_DIFF_LINES || diff.files.len() >= LARGE_DIFF_FILES {
        out.push(DeepReason::LargeDiff);
    }
    if diff.truncated || excluded.iter().any(|(_, e)| e.cut_by_size()) {
        out.push(DeepReason::NotFullyReviewed);
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// `text` cut to at most `max` characters.
fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// What [`assemble`] built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembled {
    /// Title, description and one `diff` part per reviewed file.
    pub parts: Vec<InputPart>,
    /// Files whose patch is in [`Self::parts`].
    pub reviewed: usize,
    /// Files left out, and why (in diff order). A confidential path is
    /// never named in a run: it is counted ([`Self::excluded_marked`]).
    pub excluded: Vec<(String, Exclusion)>,
    /// Confidential files left out (named nowhere, counted only).
    pub excluded_marked: usize,
    /// The code rules' deep-review reasons.
    pub deep_review: Vec<DeepReason>,
}

impl Assembled {
    /// The class the advice will have: the highest of the parts.
    pub fn class(&self) -> InputClass {
        InputClass::of_parts(self.parts.iter().map(|p| p.class))
    }

    /// Every changed file that can be read was sent (none cut by size,
    /// git's diff not cut): only then can "no finding" be a `success`.
    pub fn fully_reviewed(&self) -> bool {
        !self.deep_review.contains(&DeepReason::NotFullyReviewed)
    }
}

/// The input of the first pass (module docs). `confidential(path)`: the
/// caller's answer (marked documents at the merge base or the head; a
/// failure to read them is the caller's to treat as confidential).
pub fn assemble(
    title: &str,
    description: &str,
    diff: &Diff,
    confidential: impl Fn(&str) -> bool,
) -> Assembled {
    let mut parts = vec![InputPart::new(
        "title",
        InputClass::Internal,
        cut(title, MAX_TITLE_CHARS),
    )];
    if !description.trim().is_empty() {
        parts.push(InputPart::new(
            "description",
            InputClass::Internal,
            cut(description, MAX_DESCRIPTION_CHARS),
        ));
    }
    let mut excluded = Vec::new();
    let mut excluded_marked = 0usize;
    let mut cut_marked = false;
    let mut reviewed = 0usize;
    let mut used = 0usize;
    for f in &diff.files {
        let path = path_of(f);
        let marked = confidential(path) || f.old_path.as_deref().is_some_and(&confidential);
        let left_out = if let Some(kind) = excluded_kind(path)
            .or_else(|| f.old_path.as_deref().and_then(excluded_kind))
            .filter(|k| *k == Exclusion::SecretFile)
            .or_else(|| excluded_kind(path))
        {
            Some(kind)
        } else if f.binary || f.omitted == Some(PatchOmitted::NotUtf8) {
            Some(Exclusion::Binary)
        } else if f.omitted.is_some_and(PatchOmitted::is_cap) {
            Some(Exclusion::TooLarge)
        } else {
            None
        };
        let patch = f.patch.as_deref().unwrap_or("");
        let chars = patch.chars().count();
        let left_out = left_out.or(if chars > MAX_FILE_CHARS {
            Some(Exclusion::TooLarge)
        } else if !patch.is_empty() && used + chars > MAX_DIFF_CHARS {
            Some(Exclusion::OverBudget)
        } else {
            None
        });
        if let Some(kind) = left_out {
            if marked {
                // Never named where an agent could read it (a run).
                excluded_marked += 1;
                cut_marked |= kind.cut_by_size();
            } else {
                excluded.push((path.to_owned(), kind));
            }
            continue;
        }
        if patch.is_empty() {
            continue; // a mode change or a pure rename: nothing to read
        }
        used += chars;
        reviewed += 1;
        let class = if marked {
            InputClass::Confidential
        } else {
            InputClass::Internal
        };
        let text = match (f.status, f.old_path.as_deref()) {
            (DiffStatus::Renamed, Some(old)) => format!("(renamed from {old})\n{patch}"),
            _ => patch.to_owned(),
        };
        parts.push(InputPart::new("diff", class, text).with_path(path));
    }
    let mut deep_review = deep_review_reasons(diff, &excluded);
    if cut_marked && !deep_review.contains(&DeepReason::NotFullyReviewed) {
        deep_review.push(DeepReason::NotFullyReviewed);
        deep_review.sort_unstable();
    }
    Assembled {
        parts,
        reviewed,
        excluded,
        excluded_marked,
        deep_review,
    }
}

/// The conclusion of a checked advice (module docs). `Off` is never asked
/// for (no run at all); it reads like `Advisory`. `fully_reviewed`
/// ([`Assembled::fully_reviewed`]): a change partly sent is never a
/// `success`, whatever the model found in the part it saw.
pub fn conclusion(mode: ReviewMode, advice: &Advice, fully_reviewed: bool) -> CheckConclusion {
    let serious = advice.findings.iter().any(|f| {
        matches!(
            f.severity,
            FindingSeverity::Critical | FindingSeverity::High
        )
    });
    if mode == ReviewMode::Required && serious {
        return CheckConclusion::Failure;
    }
    if advice.findings.is_empty() && !advice.degraded && fully_reviewed {
        CheckConclusion::Success
    } else {
        CheckConclusion::Neutral
    }
}

fn name() -> Result<CheckName, ForgeError> {
    CheckName::parse(AI_REVIEW_CHECK).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "ai_review_name_invalid")
    })
}

/// `text` cut to at most `max` bytes on a character boundary (`…` when cut).
fn clip(mut text: String, max: usize) -> String {
    const MARK: char = '…';
    if text.len() <= max {
        return text;
    }
    let mut end = max.saturating_sub(MARK.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push(MARK);
    text
}

const NEVER_DECIDES: &str =
    "Advice only: the merge is decided by tests, required checks and people.";

/// The run while the request waits (`queued`) — or waits for a paused
/// gateway (`paused`).
pub fn waiting_run(paused: bool) -> Result<NewCheckRun, ForgeError> {
    let summary = if paused {
        format!("{ADVICE_LABEL}\nThe local LLM gateway is paused: the first pass waits until it is resumed. {NEVER_DECIDES}")
    } else {
        format!("{ADVICE_LABEL}\nThe first pass is queued for the local LLM. {NEVER_DECIDES}")
    };
    Ok(NewCheckRun {
        name: name()?,
        status: CheckStatus::Queued,
        conclusion: None,
        summary,
        details_url: None,
        annotations: Vec::new(),
    })
}

/// The run when the first pass failed (`error`, never a success): the
/// code and the request id only — the reader finds the rest in the logs.
pub fn failed_run(code: ErrorCode, request_id: &str) -> Result<NewCheckRun, ForgeError> {
    Ok(NewCheckRun {
        name: name()?,
        status: CheckStatus::Completed,
        conclusion: Some(CheckConclusion::Error),
        summary: clip(
            format!(
                "{ADVICE_LABEL}\nThe first pass failed: no AI advice for this head ({} — request {request_id}). It is not a success; nothing was decided. {NEVER_DECIDES}",
                code.id()
            ),
            MAX_CHECK_SUMMARY_BYTES,
        ),
        details_url: None,
        annotations: Vec::new(),
    })
}

/// The run of a head that was not reviewed because nothing in it can be
/// read by the model (only lock, generated or binary files): `neutral`.
pub fn nothing_to_review_run(
    excluded: &[(String, Exclusion)],
    excluded_marked: usize,
) -> Result<NewCheckRun, ForgeError> {
    let mut lines = vec![
        ADVICE_LABEL.to_owned(),
        format!("Not reviewed: no changed file can be read by the AI. {NEVER_DECIDES}"),
    ];
    lines.extend(excluded_lines(excluded, excluded_marked));
    Ok(NewCheckRun {
        name: name()?,
        status: CheckStatus::Completed,
        conclusion: Some(CheckConclusion::Neutral),
        summary: clip(lines.join("\n"), MAX_CHECK_SUMMARY_BYTES),
        details_url: None,
        annotations: Vec::new(),
    })
}

/// The run of a head with no common history with its target (no merge
/// base, so no diff to read): `neutral`, not reviewed.
pub fn no_common_history_run() -> Result<NewCheckRun, ForgeError> {
    Ok(NewCheckRun {
        name: name()?,
        status: CheckStatus::Completed,
        conclusion: Some(CheckConclusion::Neutral),
        summary: format!(
            "{ADVICE_LABEL}
Not reviewed: the head has no common history with its target branch, so there is no diff to read. {NEVER_DECIDES}"
        ),
        details_url: None,
        annotations: Vec::new(),
    })
}

/// The run of a head the merge request has moved on from (or that is no
/// longer open) before its answer came: `neutral`, no advice words (the
/// new head is reviewed on its own).
pub fn superseded_run() -> Result<NewCheckRun, ForgeError> {
    Ok(NewCheckRun {
        name: name()?,
        status: CheckStatus::Completed,
        conclusion: Some(CheckConclusion::Neutral),
        summary: format!(
            "{ADVICE_LABEL}\nNot shown: the merge request moved to another head (or closed) before the answer came. {NEVER_DECIDES}"
        ),
        details_url: None,
        annotations: Vec::new(),
    })
}

/// The files not sent, by path; a confidential one only counted (a run
/// is read by agent tokens too).
fn excluded_lines(excluded: &[(String, Exclusion)], marked: usize) -> Vec<String> {
    const SHOWN: usize = 20;
    let mut out: Vec<String> = excluded
        .iter()
        .take(SHOWN)
        .map(|(p, e)| format!("- not sent: {} ({})", shown(p), e.as_str()))
        .collect();
    if excluded.len() > SHOWN {
        out.push(format!("- … and {} more not sent", excluded.len() - SHOWN));
    }
    if marked > 0 {
        out.push(format!("- not sent: {marked} confidential file(s)"));
    }
    out
}

/// A path as shown: control characters replaced.
fn shown(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn relative(path: &str) -> bool {
    let drive = matches!(path.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic());
    !path.is_empty()
        && path.len() <= MAX_ANNOTATION_PATH_BYTES
        && !path.starts_with('/')
        && !drive
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path.split('/').any(|segment| segment == "..")
}

/// What labels an advice in its run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLabel<'a> {
    /// `name:tag`.
    pub model: &'a str,
    pub prompt_version: i32,
    pub advice_id: i64,
    pub request_id: &'a str,
}

/// The completed run of an advice (module docs). A confidential advice's
/// words stay out of it.
pub fn advised_run(
    mode: ReviewMode,
    advice: &Advice,
    assembled: &Assembled,
    deep_review: Option<&str>,
    label: &RunLabel<'_>,
) -> Result<NewCheckRun, ForgeError> {
    let decided = conclusion(mode, advice, assembled.fully_reviewed());
    let mut lines = vec![
        format!(
            "{ADVICE_LABEL} — model {}, prompt {PROMPT_ID} v{}, advice {} (request {}). {NEVER_DECIDES}",
            label.model, label.prompt_version, label.advice_id, label.request_id
        ),
        format!(
            "{} finding(s); {} file(s) reviewed, {} not sent.",
            advice.findings.len(),
            assembled.reviewed,
            assembled.excluded.len() + assembled.excluded_marked
        ),
    ];
    if !assembled.fully_reviewed() {
        lines.push(
            "Partly reviewed: some of the change was not sent to the AI (size), so no finding is not a success."
                .to_owned(),
        );
    }
    if advice.degraded {
        lines.push(format!(
            "Low confidence: the answer was cleaned by code (verdict withheld: {}, findings dropped for missing evidence: {}).",
            advice.summary_withheld, advice.dropped_evidence
        ));
    }
    let confidential = !advice.input_class.shown_to_agents();
    let mut annotations = Vec::new();
    if confidential {
        lines.push(format!(
            "The input holds confidential files: the advice's words are shown to people on the merge request page only ({WITHHELD_JA})."
        ));
        if deep_review.is_some() {
            lines.push("A deeper review (Claude or a person) is recommended.".to_owned());
        }
    } else {
        lines.push(String::new());
        lines.push(advice.summary.clone()); // clone: the run owns its text
        if let Some(d) = deep_review {
            lines.push(String::new());
            lines.push(d.to_owned());
        }
        if !advice.findings.is_empty() {
            lines.push(String::new());
        }
        for f in &advice.findings {
            let place = match (&f.path, f.line) {
                (Some(p), Some(l)) => format!(" {}:{l}", shown(p)),
                (Some(p), None) => format!(" {}", shown(p)),
                (None, _) => String::new(),
            };
            lines.push(format!("- {}{place} — {}", f.severity.as_str(), f.message));
        }
        for f in &advice.findings {
            let Some(path) = f.path.as_deref().filter(|p| relative(p)) else {
                continue;
            };
            let line = f.line.unwrap_or(1).max(1);
            let serious = matches!(
                f.severity,
                FindingSeverity::Critical | FindingSeverity::High
            );
            let level = match (decided, serious) {
                (CheckConclusion::Failure, true) => CheckAnnotationLevel::Failure,
                (_, true) => CheckAnnotationLevel::Warning,
                (_, false) => CheckAnnotationLevel::Notice,
            };
            annotations.push(CheckAnnotation {
                path: path.to_owned(),
                start_line: line,
                end_line: line,
                level,
                message: clip(
                    format!("[AI advice] {}: {}", f.severity.as_str(), f.message),
                    MAX_ANNOTATION_BYTES,
                ),
                title: None,
            });
            if annotations.len() >= MAX_CHECK_ANNOTATIONS {
                break;
            }
        }
    }
    lines.extend(excluded_lines(
        &assembled.excluded,
        assembled.excluded_marked,
    ));
    Ok(NewCheckRun {
        name: name()?,
        status: CheckStatus::Completed,
        conclusion: Some(decided),
        summary: clip(lines.join("\n"), MAX_CHECK_SUMMARY_BYTES),
        details_url: None,
        annotations,
    })
}

/// The newest advice as a reader may see it.
#[derive(Debug, Clone, PartialEq)]
pub enum AdviceState {
    /// None yet (off, the LLM disabled or down, or not answered).
    None,
    Shown {
        label: AdviceLabel,
        advice: Box<StoredAdvice>,
        /// Made for another head than the current one.
        stale: bool,
    },
    /// Confidential, read by an agent: that it exists, never its words.
    Withheld {
        label: AdviceLabel,
        input_class: InputClass,
        stale: bool,
    },
}

/// `GET …/merge-requests/{iid}/ai-review` (`Review::ai_review`).
#[derive(Debug, Clone, PartialEq)]
pub struct AiReviewView {
    pub iid: i64,
    /// The project's `llm.review`.
    pub mode: ReviewMode,
    pub head_sha: Option<String>,
    /// `forge/ai-review` on the current head (`None`: none).
    pub check: Option<CheckRun>,
    pub advice: AdviceState,
    /// The reader may vote 「役に立った / 誤り」 on the findings
    /// (`AiAdviceFeedback` decided by `authorize_advice` on this merge
    /// request, a person: M5 L5-6). A hint for the screen; the vote's route
    /// decides again.
    pub can_feedback: bool,
    /// The reader may ask the first pass of the current head again now
    /// (`AiReviewRerun` decided on this merge request, a person who did
    /// not declare an agent audience; the merge request open with a head,
    /// `llm.review` not `off`, its run completed and no rerun waiting). A
    /// hint: the rerun's route decides again and answers 429 past 3 an hour.
    pub can_rerun: bool,
    /// Someone asked the current head again and the worker has not picked
    /// it up yet (the screen says "asked again" and reads again).
    pub rerun_asked: bool,
    /// The reader's own verdicts on the shown advice's findings (empty when
    /// none, withheld, or not a signed-in person).
    pub my_feedback: Vec<(u8, forge_core::llm::Verdict)>,
}

/// What `POST …/ai-review/feedback` did (`Review::ai_review_feedback`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackRecorded {
    Recorded,
    Changed,
    Unchanged,
}

impl FeedbackRecorded {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Changed => "changed",
            Self::Unchanged => "unchanged",
        }
    }
}

impl crate::Review {
    /// `GET …/merge-requests/{iid}/ai-review` (`MergeRequestRead`, module
    /// docs). `declared_agent`: `Forge-Context-Audience: agent`. The
    /// advice's words are read on the advice's own subject; a subject that
    /// is not this merge request is `FG-SYS-0001
    /// reason=advice_subject_mismatch` (never shown).
    pub async fn ai_review(
        &self,
        proof: &forge_auth::Authorized<forge_auth::MergeRequestCtx>,
        declared_agent: bool,
    ) -> Result<AiReviewView, ForgeError> {
        use forge_auth::{authorize_advice, Action, AdviceReader, ResourceCtx as _};
        use forge_core::llm::{Purpose, SubjectKind};
        use forge_db::llm::{AdviceView, ReaderKind};
        if proof.action() != Action::MergeRequestRead {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "proof_action_mismatch")
                .with_context("action", proof.action().name()));
        }
        let (project, Some(iid)) = *proof.id() else {
            return Err(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_shape_mismatch")
            );
        };
        let mr = forge_db::review::get(&self.db, project, iid)
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "decided_merge_request_missing")
            })?;
        let mode = forge_db::llm::review_mode(&self.db, project).await?;
        let check = match mr.head_sha.as_deref().map(forge_core::ObjectId::parse) {
            Some(Ok(head)) => {
                let runs = forge_db::checks::runs_for(&self.db, project, &head).await?;
                runs.run(&head, &name()?)
            }
            Some(Err(_)) | None => None,
        };
        let label = forge_db::llm::latest_advice(
            &self.db,
            SubjectKind::MergeRequest,
            mr.id.get(),
            Purpose::MrReview,
        )
        .await?;
        let resource = proof.ctx().resource();
        let can_feedback = matches!(
            authorize_advice(
                proof.principal(),
                Action::AiAdviceFeedback,
                &resource,
                declared_agent,
            ),
            Ok(AdviceReader::Person)
        );
        let rerun = match mr.head_sha.as_deref() {
            Some(head)
                if mode != forge_core::llm::ReviewMode::Off
                    && mr.state == forge_core::MergeRequestState::Open =>
            {
                forge_db::llm::ops::rerun_state(&self.db, project, mr.id, head).await?
            }
            _ => forge_db::llm::ops::RerunState::NotDone,
        };
        let rerun_asked = rerun == forge_db::llm::ops::RerunState::Asked;
        let can_rerun = rerun == forge_db::llm::ops::RerunState::Possible
            && !declared_agent
            && forge_auth::authorize(proof.principal(), Action::AiReviewRerun, &resource).is_ok();
        let Some(label) = label else {
            return Ok(AiReviewView {
                iid,
                mode,
                head_sha: mr.head_sha,
                check,
                advice: AdviceState::None,
                can_feedback,
                can_rerun,
                rerun_asked,
                my_feedback: Vec::new(),
            });
        };
        let stale = label.head_sha.is_none() || label.head_sha != mr.head_sha;
        let viewed = forge_db::llm::advice_decided(&self.db, label.id, |subject| {
            if subject.kind != SubjectKind::MergeRequest
                || subject.id != Some(mr.id.get())
                || subject.project != Some(project)
            {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "advice_subject_mismatch")
                    .with_context("advice_id", subject.advice_id));
            }
            authorize_advice(
                proof.principal(),
                Action::MergeRequestRead,
                &resource,
                declared_agent,
            )
            .map(|r| match r {
                AdviceReader::Person => ReaderKind::Person,
                AdviceReader::Agent => ReaderKind::Agent,
            })
        })
        .await?;
        let my_feedback = match (&viewed, proof.principal().user()) {
            (Some(AdviceView::Shown(_)), Some(user)) if can_feedback => {
                forge_db::llm::my_feedback(&self.db, label.id, user).await?
            }
            _ => Vec::new(),
        };
        let advice = match viewed {
            None => AdviceState::None,
            Some(AdviceView::Shown(advice)) => AdviceState::Shown {
                label,
                advice,
                stale,
            },
            Some(AdviceView::Withheld { input_class, .. }) => AdviceState::Withheld {
                label,
                input_class,
                stale,
            },
        };
        Ok(AiReviewView {
            iid,
            mode,
            head_sha: mr.head_sha,
            check,
            advice,
            can_feedback,
            can_rerun,
            rerun_asked,
            my_feedback,
        })
    }

    /// `POST …/merge-requests/{iid}/ai-review/feedback` (`AiAdviceFeedback`,
    /// M5 L5-6, plan 19 §4): a person's verdict on finding `finding` of
    /// advice `advice_id`, one per person and finding (a new verdict
    /// replaces theirs). The vote is decided again on the advice's own
    /// subject by `authorize_advice`: an advice that is not this merge
    /// request's, or no such advice or finding, is `FG-REPO-0005`
    /// not found (`reason=advice_not_found`, existence never leaks); an
    /// agent audience (a person's token that declared
    /// `Forge-Context-Audience: agent`) is refused (`reason=agent_audience`):
    /// an agent's vote would skew the measure of the advice.
    pub async fn ai_review_feedback(
        &self,
        proof: &forge_auth::Authorized<forge_auth::MergeRequestCtx>,
        declared_agent: bool,
        advice_id: i64,
        finding: u8,
        verdict: forge_core::llm::Verdict,
    ) -> Result<FeedbackRecorded, ForgeError> {
        use forge_auth::{authorize_advice, Action, AdviceReader, ResourceCtx as _};
        use forge_core::llm::SubjectKind;
        use forge_db::llm::{FeedbackOutcome, ReaderKind};
        if proof.action() != Action::AiAdviceFeedback {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "proof_action_mismatch")
                .with_context("action", proof.action().name()));
        }
        let (project, Some(iid)) = *proof.id() else {
            return Err(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_shape_mismatch")
            );
        };
        let Some(user) = proof.principal().user() else {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "decided_vote_without_user"));
        };
        let mr = forge_db::review::get(&self.db, project, iid)
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "decided_merge_request_missing")
            })?;
        let not_found =
            || ForgeError::new(ErrorCode::NotFound).with_context("reason", "advice_not_found");
        let resource = proof.ctx().resource();
        let outcome = forge_db::llm::record_feedback_decided(
            &self.db,
            advice_id,
            Some(finding),
            user,
            verdict,
            |subject| {
                if subject.kind != SubjectKind::MergeRequest
                    || subject.id != Some(mr.id.get())
                    || subject.project != Some(project)
                {
                    return Err(not_found().with_context("advice_id", subject.advice_id));
                }
                authorize_advice(
                    proof.principal(),
                    Action::AiAdviceFeedback,
                    &resource,
                    declared_agent,
                )
                .map(|r| match r {
                    AdviceReader::Person => ReaderKind::Person,
                    AdviceReader::Agent => ReaderKind::Agent,
                })
            },
        )
        .await?;
        match outcome {
            FeedbackOutcome::Recorded => Ok(FeedbackRecorded::Recorded),
            FeedbackOutcome::Changed => Ok(FeedbackRecorded::Changed),
            FeedbackOutcome::Unchanged => Ok(FeedbackRecorded::Unchanged),
            FeedbackOutcome::NoAdvice => Err(not_found()
                .with_context("advice_id", advice_id)
                .with_context("finding_index", i64::from(finding))),
            FeedbackOutcome::NotAllowed => {
                Err(ForgeError::new(ErrorCode::Forbidden).with_context("reason", "agent_audience"))
            }
        }
    }
}

#[cfg(test)]
#[path = "ai_review_tests.rs"]
mod tests;
