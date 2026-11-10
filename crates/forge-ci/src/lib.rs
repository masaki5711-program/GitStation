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
    Definition, Job, Rules, MAX_ARTIFACT_PATHS, MAX_BRANCHES, MAX_BYTES, MAX_JOBS, MAX_NEEDS,
    MAX_SCRIPT_LINES, MAX_SECRETS, MAX_STAGES, MAX_TAGS, MAX_TIMEOUT_SECS,
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

    pub(crate) fn about(line: usize, reason: &'static str, subject: impl Into<String>) -> Self {
        Self {
            line,
            reason,
            subject: Some(subject.into()),
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

/// What the lint knows about the repository beyond the file.
#[derive(Debug, Clone, Default)]
pub struct LintContext {
    /// The repository's protected branch patterns (exact names, or a prefix
    /// ending in `*`). `None`: unknown (a local check) — then a job asking
    /// for secrets may only name exact branches, never a wildcard.
    pub protected_branches: Option<Vec<String>>,
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
