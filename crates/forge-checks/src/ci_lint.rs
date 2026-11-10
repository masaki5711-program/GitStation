//! `forge/ci-lint` (plan 14 C1, plan 04 P-1): the lint of a merge request
//! version's `.forge-ci.yml`. Written by the `check.spec_index` job beside
//! `forge/doc-lint` ([`crate::spec_checks`]): no other job kind.
//!
//! **Inputs** ([`read_input`]): the file at the root of the head, read
//! through the browse API's `small_file` — `ls-tree` decides what the entry
//! is (a symlink is never followed), a file over forge-ci's
//! [`forge_ci::MAX_BYTES`] is not read at all, and every git call has the
//! repository's browse timeout; and the project's protected branch patterns
//! (`forge_db::protected_refs::rules_for`, the snapshot a push decides on),
//! which [`branch_patterns`] turns into what forge-ci compares. The lint is
//! `forge_ci::check` with `LintContext::with_protected_branches` — never
//! the local check's "nothing is protected".
//!
//! **Conclusions**: `success` (no problem; also no `.forge-ci.yml` at all:
//! nothing to check, as `forge/spec-lint` without specifications), and
//! `failure` (`FG-CI-0001`: each problem as `.forge-ci.yml:<line> <reason>
//! — <subject>`, at most forge-ci's [`forge_ci::MAX_PROBLEMS`] and its
//! `too_many_problems`; the file not a regular file: `not_a_file`; over the
//! limit: `too_large`). Subjects are forge-ci's (cut to 64 characters; a
//! malformed secret name is never echoed, and `secret_on_unprotected_ref`
//! names the branch pattern, not the secret).

use forge_ci::{LintContext, Problem};
use forge_core::checks::{CheckAnnotationLevel, CheckConclusion, CI_LINT_CHECK};
use forge_core::{ProjectId, ProtectionRules, RepoId};
use forge_db::checks::NewCheckRun;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, RepoPath, SmallFile};

use crate::spec_checks::{annotation, completed};

/// The pipeline definition's path at the root of a commit.
pub const CI_FILE: &str = ".forge-ci.yml";

/// The protected ref prefix of a branch.
const HEADS: &str = "refs/heads/";

/// What `.forge-ci.yml` at a commit is (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiFile {
    /// No entry at the path: nothing to check.
    Absent,
    /// A directory, a symlink or a submodule.
    NotAFile,
    /// Over [`forge_ci::MAX_BYTES`]: not read.
    TooLarge,
    /// The whole file.
    Found(Vec<u8>),
}

/// Everything the lint reads (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiLintInput {
    pub file: CiFile,
    /// The project's protected branch patterns ([`branch_patterns`]).
    pub protected: Vec<String>,
}

/// The run and, when it fails, the registered error for the log
/// (`FG-CI-0001` with the first problem's `reason` and `line`).
#[derive(Debug)]
pub struct CiLint {
    pub run: NewCheckRun,
    pub invalid: Option<ForgeError>,
}

fn ci_path() -> Result<RepoPath, ForgeError> {
    RepoPath::parse(CI_FILE).map_err(|rule| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "ci_file_path_invalid")
            .with_context("rule", rule)
    })
}

/// The protected branch patterns forge-ci can compare, from the project's
/// rules: `refs/heads/<pattern>` without the prefix, as written and
/// case-folded (a rule matches ignoring case, so both spellings are
/// protected). Tag rules protect no branch. A `*` anywhere but at the end
/// is left out: forge-ci compares only a trailing `*` — those branches are
/// then treated as unprotected (a false alarm, never a secret let through).
///
/// forge-ci compares case-sensitively while a rule matches ignoring case:
/// with both spellings passed, a job pattern in another case (`MAIN` against
/// a rule `refs/heads/main`) is still not covered — a false alarm
/// (`secret_on_unprotected_ref`), never a miss.
pub fn branch_patterns(rules: &ProtectionRules) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for rule in rules.rules() {
        for raw in [rule.pattern.as_str(), rule.pattern.key()] {
            let Some(branch) = raw.strip_prefix(HEADS) else {
                continue;
            };
            let star_inside = branch.find('*').is_some_and(|at| at + 1 != branch.len());
            if branch.is_empty() || star_inside || out.iter().any(|p| p == branch) {
                continue;
            }
            out.push(branch.to_owned());
        }
    }
    out
}

/// Read `.forge-ci.yml` at `commit` of `repo` and the protected branches of
/// `project` (module docs). Errors: forge-git's (`FG-GIT-…`: the browse
/// timeout and caps), with the commit; the database's (`FG-DB-…`, and
/// `FG-DB-0020` for a stored rule this build cannot read — never a partial
/// list).
pub async fn read_input(
    db: &Db,
    browse: &Browser,
    project: ProjectId,
    repo: RepoId,
    commit: &str,
) -> Result<CiLintInput, ForgeError> {
    let max = u64::try_from(forge_ci::MAX_BYTES).unwrap_or(u64::MAX);
    let file = browse
        .small_file(repo, commit, &ci_path()?, max)
        .await
        .map_err(|e| e.with_context("commit", commit.to_owned()))?;
    let file = match file {
        SmallFile::Absent => CiFile::Absent,
        SmallFile::NotAFile => CiFile::NotAFile,
        SmallFile::TooLarge => CiFile::TooLarge,
        SmallFile::Found(bytes) => CiFile::Found(bytes),
    };
    let rules = forge_db::protected_refs::rules_for(db, project).await?;
    Ok(CiLintInput {
        file,
        protected: branch_patterns(&rules),
    })
}

/// The problems of `input` (`None`: no file, nothing to check).
fn problems(input: &CiLintInput) -> Option<Vec<Problem>> {
    let whole = |reason| Problem {
        line: 0,
        reason,
        subject: None,
    };
    match &input.file {
        CiFile::Absent => None,
        CiFile::NotAFile => Some(vec![whole("not_a_file")]),
        CiFile::TooLarge => Some(vec![whole("too_large")]),
        CiFile::Found(bytes) => {
            // clone: the context owns its list
            let ctx = LintContext::with_protected_branches(input.protected.clone());
            Some(forge_ci::lint(bytes, &ctx))
        }
    }
}

fn problem_line(p: &Problem) -> String {
    let code = ErrorCode::CiConfigInvalid.id();
    match &p.subject {
        Some(s) => format!("{CI_FILE}:{} {} — {s} [{code}]", p.line, p.reason),
        None => format!("{CI_FILE}:{} {} [{code}]", p.line, p.reason),
    }
}

/// The `forge/ci-lint` run of `input` (module docs).
pub fn assess_ci_lint(input: &CiLintInput) -> Result<CiLint, ForgeError> {
    let Some(found) = problems(input) else {
        let run = completed(
            CI_LINT_CHECK,
            CheckConclusion::Success,
            &[format!(
                "No {CI_FILE} at this commit: nothing to check (no pipeline is defined)."
            )],
            Vec::new(),
        )?;
        return Ok(CiLint { run, invalid: None });
    };
    if found.is_empty() {
        let run = completed(
            CI_LINT_CHECK,
            CheckConclusion::Success,
            &[format!(
                "{CI_FILE} keeps every rule ({} protected branch pattern(s) known).",
                input.protected.len()
            )],
            Vec::new(),
        )?;
        return Ok(CiLint { run, invalid: None });
    }
    let mut lines = vec![format!(
        "{} problem(s) in {CI_FILE} [{}]:",
        found.len(),
        ErrorCode::CiConfigInvalid.id()
    )];
    lines.extend(found.iter().map(problem_line));
    let annotations = found
        .iter()
        .map(|p| {
            annotation(
                CI_FILE,
                p.line,
                CheckAnnotationLevel::Failure,
                problem_line(p),
            )
        })
        .collect();
    let run = completed(CI_LINT_CHECK, CheckConclusion::Failure, &lines, annotations)?;
    Ok(CiLint {
        run,
        invalid: Some(forge_ci::invalid(&found)),
    })
}

#[cfg(test)]
#[path = "ci_lint_tests.rs"]
mod tests;
