//! The merge queue's train lint (plan 18 S1, ADR-0037 (b)): [`TrainLint`],
//! the implementation of `forge_core::train_lint::TrainDocLint` that
//! `forge serve` injects into forge-review's queue worker.
//!
//! It answers for the tree the queue is about to land — the train's last
//! step — with the reading and the lint of the head's `forge/doc-lint`
//! ([`crate::doc_lint`]: `forge_doc::lint::lint`, the function of
//! `gstn doc lint`, on the same bytes). No cache yet: every `.md` of a
//! tree is read each time (plan 18 S7 makes it incremental).
//!
//! 1. **Strict?** `.forge/repo.yml` at the base
//!    (`merge_queue.doc_lint_strict`). Absent: not strict. A file that
//!    cannot be read as a valid one (`FG-REPO-0024`) never relaxes the
//!    rule: strict, logged at info.
//! 2. **Applies?** A strict repository: always. Otherwise only when the
//!    train changes a `.md` file or a type definition (one `diff-tree`
//!    between the base and the last step; too many paths to tell: it
//!    applies).
//! 3. **The last step** is read whole within [`TRAIN_DOC_LIMITS`] — a
//!    bound hit is "could not be linted", never a verdict on a part — and
//!    linted; its `forge/doc-lint` run (the head's assessment, with a line
//!    naming the train's base) is recorded on that commit, which is the
//!    target's tip once the train lands and the evidence a removed merge
//!    request links to.
//! 4. **The verdict**: the problems of the last step the base does not
//!    have (same path, rule and referenced id; counted, so a second broken
//!    reference to the same id in the same file is new). Broken references
//!    lowered by the repository's migration setting (plan 13 Q13) do not
//!    count. New ones: a binary search over the steps for one that is red
//!    while the step before it is not.
//!
//! Every failure is `FG-DOC-0002` with `scope=train`: `reason=train_budget`
//! past the caller's budget, `reason=read_failed` with the cause's code
//! (`cause`) or the bound hit (`bound`) otherwise. The queue never lands a
//! train on an error.

use std::collections::BTreeMap;
use std::time::Duration;

use forge_core::repo_config::RepoConfigProblem;
use forge_core::train_lint::{
    TrainDocLint, TrainLintFuture, TrainLintInput, TrainLintProblem, TrainLintVerdict,
    MAX_LISTED_PROBLEMS, REASON_BUDGET, REASON_READ_FAILED,
};
use forge_core::{ObjectId, RepoId, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::record_forge_check_run;
use forge_db::Db;
use forge_doc::lint::FileReport;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;
use forge_git::review_inputs::ChangedPaths;
use forge_git::{Git, StorageRoot};

use crate::doc_lint::{
    assess_doc_lint_reports, is_document_file, lint_tree, lowered, read_tree, DocLimits, DocTree,
};
use crate::spec_checks::{LOOKUP_TIMEOUT, WRITE_TIMEOUT};
use crate::spec_refs::{read_repo_config, RepoConfigRead};

/// The bounds of one tree's reading for a train: ten times the head's
/// `.md` files and twice its bytes (the head's run turns `neutral` past
/// its bounds; a train cannot land on a part, so its bounds are wider).
pub const TRAIN_DOC_LIMITS: DocLimits = DocLimits {
    max_tree_entries: 50_000,
    max_files: 20_000,
    max_large_files: 100,
    max_total_bytes: 64 * 1024 * 1024,
    max_doctypes: 64,
};

/// Changed paths listed at most to decide whether the lint applies (more:
/// it applies).
const MAX_CHANGED_PATHS: usize = 10_000;

/// The rule name standing for a file's problems past the per-file bound
/// (their rules are not known): compared by count.
const DROPPED_RULE: &str = "problems_dropped";

/// The directory of the type definitions, with its separator: any change
/// below it makes the lint apply.
const DOCTYPES_PREFIX: &str = ".forge/doctypes/";

/// `FG-DOC-0002 scope=train` with `reason`.
fn unavailable(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::DocIndexBounded)
        .with_context("scope", "train")
        .with_context("reason", reason)
}

/// `reason=read_failed` for `cause` (its code in `cause`, itself the
/// source: logged once by the queue).
fn read_failed(cause: ForgeError) -> ForgeError {
    unavailable(REASON_READ_FAILED)
        .with_context("cause", cause.code().id())
        .with_source(cause)
}

/// `work` within `limit` (`FG-WEB-0002` with `operation` past it).
async fn within<T>(
    operation: &'static str,
    limit: Duration,
    work: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    tokio::time::timeout(limit, work).await.unwrap_or_else(|_| {
        Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", operation)
            .with_context("timeout_ms", limit.as_millis()))
    })
}

/// Whether a change of `path` can change the document lint's answer.
fn is_lint_input(path: &str) -> bool {
    is_document_file(path) || path.starts_with(DOCTYPES_PREFIX)
}

/// The problems of one tree as the verdict counts them: by what makes a
/// problem the same one on two trees, without the ones `migration` lowers.
type Problems = BTreeMap<TrainLintProblem, usize>;

fn problems_of(reports: &[FileReport], migration: Option<&str>) -> Problems {
    let mut counted = Problems::new();
    let mut count = |problem: TrainLintProblem, n: usize| {
        let slot = counted.entry(problem).or_insert(0);
        *slot = slot.saturating_add(n);
    };
    for r in reports {
        for p in r.problems.iter().filter(|p| !lowered(migration, p.rule)) {
            count(
                TrainLintProblem {
                    path: r.path.clone(), // clone: the verdict owns its problems
                    rule: p.rule.to_owned(),
                    reference: p.subject.clone(), // clone: as above
                },
                1,
            );
        }
        if r.problems_dropped > 0 {
            count(
                TrainLintProblem {
                    path: r.path.clone(), // clone: as above
                    rule: DROPPED_RULE.to_owned(),
                    reference: None,
                },
                r.problems_dropped,
            );
        }
    }
    counted
}

/// The problems of `of` that `base` does not have (as often), in order.
fn new_against(of: &Problems, base: &Problems) -> Vec<TrainLintProblem> {
    of.iter()
        .filter(|(problem, n)| **n > base.get(*problem).copied().unwrap_or(0))
        .map(|(problem, _)| problem.clone()) // clone: the verdict owns its problems
        .collect()
}

fn listed(mut problems: Vec<TrainLintProblem>) -> Vec<TrainLintProblem> {
    problems.truncate(MAX_LISTED_PROBLEMS);
    problems
}

/// One commit's tree, read whole and linted.
struct Linted {
    tree: DocTree,
    reports: Vec<FileReport>,
}

/// The documents of `commit` read within [`TRAIN_DOC_LIMITS`] and linted
/// as `gstn doc lint` lints a working tree of the same files. A bound hit
/// is an error (`FG-DOC-0002 scope=train reason=read_failed` with
/// `bound`): the answer is never about a part.
pub async fn lint_commit(
    browse: &Browser,
    repo: RepoId,
    commit: &ObjectId,
) -> Result<Vec<FileReport>, ForgeError> {
    lint_at(browse, repo, commit)
        .await
        .map(|linted| linted.reports)
}

/// `commit`'s documents, whole (a bound hit is an error), linted off the
/// async threads (parsing thousands of files is CPU work).
async fn lint_at(browse: &Browser, repo: RepoId, commit: &ObjectId) -> Result<Linted, ForgeError> {
    let tree = read_tree(browse, repo, commit.as_str(), &TRAIN_DOC_LIMITS)
        .await
        .map_err(read_failed)?;
    if let Some(bound) = tree.incomplete {
        return Err(unavailable(REASON_READ_FAILED)
            .with_context("bound", bound)
            .with_context("commit", commit.as_str().to_owned()));
    }
    let linted = tokio::task::spawn_blocking(move || {
        let reports = lint_tree(&tree);
        Linted { tree, reports }
    })
    .await;
    linted.map_err(|e| {
        read_failed(
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "train_lint_task_failed")
                .with_context("panicked", e.is_panic()),
        )
    })
}

/// The train lint on the server's repositories and database (module docs).
#[derive(Clone)]
pub struct TrainLint {
    db: Db,
    git: Git,
    storage: StorageRoot,
    browse: Browser,
}

impl std::fmt::Debug for TrainLint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrainLint").finish_non_exhaustive()
    }
}

impl TrainLint {
    pub fn new(db: Db, git: Git, storage: StorageRoot, browse: Browser) -> Self {
        Self {
            db,
            git,
            storage,
            browse,
        }
    }

    /// Step 1 of the module docs.
    async fn strict(
        &self,
        repo: RepoId,
        base: &ObjectId,
        request_id: &RequestId,
    ) -> Result<bool, ForgeError> {
        let read = read_repo_config(&self.browse, repo, base.as_str())
            .await
            .map_err(read_failed)?;
        Ok(match read {
            RepoConfigRead::Absent => false,
            RepoConfigRead::Valid(config) => config.merge_queue.doc_lint_strict,
            RepoConfigRead::Invalid(RepoConfigProblem { line, reason }) => {
                tracing::info!(
                    target: "forge_checks",
                    event = "forge.train_lint.repo_config_invalid",
                    error_code = ErrorCode::RepoConfigInvalid.id(),
                    request_id = %request_id,
                    repo_id = repo.get(),
                    base_sha = base.as_str(),
                    line,
                    reason,
                    "the base's .forge/repo.yml is not valid: the train lint treats the \
                     repository as strict"
                );
                true
            }
        })
    }

    /// Step 2 of the module docs for a repository that is not strict.
    async fn changes_documents(
        &self,
        input: &TrainLintInput<'_>,
        tip: &ObjectId,
    ) -> Result<bool, ForgeError> {
        let changed = self
            .git
            .changed_paths(
                &self.storage,
                input.repo,
                input.base,
                tip,
                MAX_CHANGED_PATHS,
                input.budget,
            )
            .await
            .map_err(read_failed)?;
        Ok(match changed {
            ChangedPaths::Complete(paths) => paths.iter().any(|p| is_lint_input(p)),
            // Too many paths, or one that is not text: it cannot be told,
            // so the tree is linted.
            ChangedPaths::TooMany | ChangedPaths::NonUtf8 => true,
        })
    }

    /// Step 3's record: the run of the last step on that commit.
    async fn record(
        &self,
        input: &TrainLintInput<'_>,
        tip: &ObjectId,
        linted: &Linted,
        migration: Option<&str>,
        new: usize,
    ) -> Result<(), ForgeError> {
        let preface = format!(
            "Merge queue: the tree of a train built on {} — {new} problem(s) the base does not \
             have (the queue's verdict; the list below is every problem of this tree).",
            input.base.as_str()
        );
        let run = assess_doc_lint_reports(&linted.tree, &linted.reports, migration, Some(preface))
            .map_err(read_failed)?;
        let ctx = ChangeContext::new(Actor::System, input.request_id.clone()); // clone: the change owns its id
        within(
            "train_lint_record",
            WRITE_TIMEOUT,
            record_forge_check_run(&self.db, &ctx, (input.project, tip), &run),
        )
        .await
        .map(drop)
        .map_err(read_failed)
    }

    /// The lint without its budget (module docs).
    async fn lint(&self, input: TrainLintInput<'_>) -> Result<TrainLintVerdict, ForgeError> {
        let Some((tip, earlier)) = input.steps.split_last() else {
            return Err(read_failed(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "train_without_steps"),
            ));
        };
        let strict = self
            .strict(input.repo, input.base, input.request_id)
            .await?;
        if !strict && !self.changes_documents(&input, tip).await? {
            return Ok(TrainLintVerdict::NotApplicable);
        }
        let migration = within(
            "doc_lint_migration_load",
            LOOKUP_TIMEOUT,
            forge_db::checks::doc_lint_migration::in_force(&self.db, input.project),
        )
        .await
        .map_err(read_failed)?;
        let migration = migration.as_deref();
        let last = lint_at(&self.browse, input.repo, tip).await?;
        let at_tip = problems_of(&last.reports, migration);
        // A clean last step needs no base: nothing can be new.
        let at_base = if at_tip.is_empty() {
            Problems::new()
        } else {
            let base = lint_at(&self.browse, input.repo, input.base).await?;
            problems_of(&base.reports, migration)
        };
        let mut new = new_against(&at_tip, &at_base);
        self.record(&input, tip, &last, migration, new.len())
            .await?;
        if at_tip.is_empty() {
            return Ok(TrainLintVerdict::Clean);
        }
        if new.is_empty() {
            return Ok(if strict {
                TrainLintVerdict::BaseNotClean {
                    problems: listed(at_tip.into_keys().collect()),
                }
            } else {
                TrainLintVerdict::Clean
            });
        }
        // `earlier[..clean]` add nothing (none: the base itself); step
        // `bad` is red. A step in between is linted until they meet.
        let (mut clean, mut bad) = (0usize, earlier.len());
        while clean < bad {
            let middle = clean.saturating_add((bad - clean) / 2);
            let Some(step) = earlier.get(middle) else {
                break;
            };
            let at_step = problems_of(
                &lint_at(&self.browse, input.repo, step).await?.reports,
                migration,
            );
            let new_at_step = new_against(&at_step, &at_base);
            if new_at_step.is_empty() {
                clean = middle.saturating_add(1);
            } else {
                bad = middle;
                new = new_at_step;
            }
        }
        Ok(TrainLintVerdict::NewProblems {
            first_bad_step: bad,
            problems: listed(new),
        })
    }
}

impl TrainDocLint for TrainLint {
    fn lint_train<'a>(&'a self, input: TrainLintInput<'a>) -> TrainLintFuture<'a> {
        Box::pin(async move {
            let budget = input.budget;
            match tokio::time::timeout(budget, self.lint(input)).await {
                Ok(verdict) => verdict,
                Err(_) => {
                    Err(unavailable(REASON_BUDGET).with_context("budget_ms", budget.as_millis()))
                }
            }
        })
    }
}

#[cfg(test)]
#[path = "train_lint_tests.rs"]
mod tests;
