//! forge CI pipeline definitions (plan 14 C1, plan 04 P-1〜P-3): the
//! `.forge-ci.yml` file — parse, lint and plan. **Pure**: no I/O, no clock,
//! no randomness — bytes in, values out — so the server and a local tool can
//! give the same answer for the same bytes.
//!
//! - [`check`] parses and lints a file into a [`Definition`] (only a file
//!   without problems becomes one) or every [`Problem`] found, each with a
//!   stable `reason` and its line.
//! - [`plan()`] turns a [`Definition`] into the deterministic execution plan
//!   for one [`Trigger`] (a push to a branch, or a merge request).
//! - [`invalid`] maps problems to the registered `FG-CI-0001`.
//!
//! Scripts are text handed to the runner later (C4); nothing here interprets,
//! expands or executes them.
//!
//! Dependency direction (CLAUDE.md contract 9): `forge-ci → forge-doc →
//! forge-errors` (forge-doc for its strict YAML tree), checked by
//! `tests/purity.rs`.

mod definition;
mod graph;
pub mod plan;

pub use definition::{
    Definition, Job, Rules, Service, MAX_ARTIFACT_PATHS, MAX_BRANCHES, MAX_BYTES, MAX_JOBS,
    MAX_NEEDS, MAX_SCRIPT_LINES, MAX_SCRIPT_LINE_BYTES, MAX_SECRETS, MAX_SERVICES, MAX_SERVICE_ENV,
    MAX_SERVICE_TMPFS, MAX_STAGES, MAX_TAGS, MAX_TIMEOUT_SECS,
};
pub use plan::{plan, Event, Plan, PlannedJob, Trigger};

use std::fmt;

use forge_errors::{ErrorCode, ForgeError};

/// One problem in a pipeline definition: the line (1-based; 0 for the whole
/// file), a stable `reason` (the catalog entry of FG-CI-0001 lists them) and
/// the subject it is about (a key, job, stage, need or secret name), if any.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Problem {
    pub line: usize,
    pub reason: &'static str,
    pub subject: Option<String>,
}

impl Problem {
    pub(crate) fn new(line: usize, reason: &'static str) -> Self {
        Self {
            line,
            reason,
            subject: None,
        }
    }

    /// A problem about `subject`, cut to [`MAX_SUBJECT_CHARS`] characters
    /// (a subject is the file's own text: it must stay short in a log).
    pub(crate) fn about(line: usize, reason: &'static str, subject: impl Into<String>) -> Self {
        let mut subject: String = subject.into();
        if let Some((cut, _)) = subject.char_indices().nth(MAX_SUBJECT_CHARS) {
            subject.truncate(cut);
        }
        Self {
            line,
            reason,
            subject: Some(subject),
        }
    }
}

impl fmt::Display for Problem {
    /// `<line> <reason>` and ` — <subject>` when there is one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.line, self.reason)?;
        if let Some(s) = &self.subject {
            write!(f, " — {s}")?;
        }
        Ok(())
    }
}

/// At most this many problems are reported; then one `too_many_problems`.
pub const MAX_PROBLEMS: usize = 100;
/// A problem's subject is cut to this many characters.
pub const MAX_SUBJECT_CHARS: usize = 64;

/// What the lint knows about the repository beyond the file. There is no
/// default: the caller says whether it knows the protected branches.
#[derive(Debug, Clone)]
pub struct LintContext {
    /// The repository's protected branch patterns (exact names, or a prefix
    /// ending in `*`); `None` for [`LintContext::local_check`].
    protected_branches: Option<Vec<String>>,
    /// [`LintContext::pinned_by_server`]: the secret-protection rule was
    /// decided by the server when it planned these bytes.
    secrets_decided: bool,
}

impl LintContext {
    /// The repository's protected branch patterns are known (the server
    /// passes the real list; `gstn ci lint --protected` passes the user's).
    /// A secret job's branch pattern must be one of them, or an exact name
    /// one of them matches (deliberately conservative: a wildcard pattern
    /// counts only when the same wildcard is protected).
    pub fn with_protected_branches(patterns: Vec<String>) -> Self {
        Self {
            protected_branches: Some(patterns),
            secrets_decided: false,
        }
    }

    /// A local check that does not know the protected branches: no branch
    /// counts as protected, so every branch pattern of a job with secrets
    /// is `secret_protection_unknown` (pass the list to check it).
    pub fn local_check() -> Self {
        Self {
            protected_branches: None,
            secrets_decided: false,
        }
    }

    /// The runner's reading of bytes the server already linted and planned
    /// with the real protected branches (M4 C4): the runner holds them only
    /// when their sha256 equals the pipeline's `definition_hash`, and
    /// whether a job may get secrets is the server's `secrets_allowed`, so
    /// the secret-protection rule (which needs the protected branches the
    /// runner does not know) is not checked again. Every other rule is.
    /// No branch counts as protected.
    pub fn pinned_by_server() -> Self {
        Self {
            protected_branches: None,
            secrets_decided: true,
        }
    }

    /// Whether `branch` is protected by the known patterns (always `false`
    /// for [`LintContext::local_check`]): the caller's [`Trigger::protected`].
    pub fn is_protected(&self, branch: &str) -> bool {
        self.protected_branches
            .as_deref()
            .is_some_and(|list| list.iter().any(|p| definition::pattern_matches(p, branch)))
    }

    pub(crate) fn secrets_decided(&self) -> bool {
        self.secrets_decided
    }

    pub(crate) fn protected_branches(&self) -> Option<&[String]> {
        self.protected_branches.as_deref()
    }
}

/// Parse and lint `bytes`: the definition, or every problem found, sorted by
/// line, reason and subject.
pub fn check(bytes: &[u8], ctx: &LintContext) -> Result<Definition, Vec<Problem>> {
    let mut problems = Vec::new();
    let parsed = definition::parse(bytes, &mut problems);
    if let Some(def) = &parsed {
        graph::lint(def, ctx, &mut problems);
    }
    match parsed {
        Some(def) if problems.is_empty() => Ok(def),
        _ => {
            problems.sort();
            problems.dedup();
            if problems.len() > MAX_PROBLEMS {
                problems.truncate(MAX_PROBLEMS);
                problems.push(Problem::new(0, "too_many_problems"));
            }
            Err(problems)
        }
    }
}

/// Only the problems of [`check`] (empty: the file is valid).
pub fn lint(bytes: &[u8], ctx: &LintContext) -> Vec<Problem> {
    check(bytes, ctx).err().unwrap_or_default()
}

/// The registered error for an invalid definition (or plan): `FG-CI-0001`
/// with the first problem's `reason` and `line`, and the number of problems.
pub fn invalid(problems: &[Problem]) -> ForgeError {
    let mut err =
        ForgeError::new(ErrorCode::CiConfigInvalid).with_context("problems", problems.len());
    if let Some(first) = problems.first() {
        err = err
            .with_context("reason", first.reason)
            .with_context("line", first.line);
    }
    err
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "prop_tests.rs"]
mod prop_tests;
