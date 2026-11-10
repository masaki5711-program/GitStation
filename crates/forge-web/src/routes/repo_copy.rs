//! The safety net of plan 06 D-5 on whole copies of a repository
//! (ADR-0026, decision of 2026-11-10): an AI agent audience
//! (`Principal::is_agent_audience`: a token issued with `is_agent`, a
//! service account, a bot) may clone, fetch or download an archive of a
//! repository only while its default branch's tip has **no** confidential
//! file (`forge_checks::confidential`, the rule of every read of file
//! content). The operating rule stays first: confidential documents live in
//! repositories the agent's account cannot read; this catches the
//! repository that broke it.
//!
//! The route measures ([`measure`]) and `authorize()` decides
//! (`forge_auth::authorize_repo_copy`: `RepoRead` on `Resource::RepoCopy`,
//! after the dispatcher's `RepoRead` proof) — the route holds no rule:
//!
//! - a person is not measured (`CopyMark::NotMeasured`; never refused);
//! - no ref at all: `Open` (nothing to copy);
//! - `HEAD` names a branch that is not born while other refs exist:
//!   `Unborn` (nothing to measure against — the read routes withhold every
//!   document then; here the whole copy is refused, fail closed);
//! - the tip's confidential paths: none → `Open`, some (or a bound left a
//!   `.md` unread) → `Confidential` / `Undecided`;
//! - the measurement failed (its 20 s timeout `FG-GIT-0002
//!   operation=confidential_paths`, a git error) or a bound that left a
//!   `.md` unread (`FG-DOC-0002`): `Undecided`, refused —
//!   the cause is carried in the refusal's log line (`check_error`), never
//!   logged a second time.
//!
//! Only the default branch's tip is looked at: history and other branches
//! are not (the operating rule covers them).
//!
//! **Cache**: the answer at a commit never changes (a commit's tree is
//! fixed), so `Open` / `Confidential` are kept per (repository, tip
//! commit) in memory — at most [`MAX_MARKS`] entries, one tip per
//! repository (a new tip drops the repository's older one), the oldest
//! dropped first. A push moves the tip and so the key; the ref list itself
//! is the browse cache every push drops. `Undecided` is never kept.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use forge_auth::{authorize_repo_copy, Authorized, CopyMark, ProjectCtx};
use forge_checks::confidential::{confidential_at, ConfidentialLimits};
use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};

use crate::routes::git::SmartHttp;

/// Kept answers at most (one per repository).
pub const MAX_MARKS: usize = 4096;
/// Confidential paths a refusal's log line names at most.
const LOGGED_PATHS: usize = 5;

/// One measurement.
#[derive(Debug)]
pub(crate) struct Measured {
    pub(crate) mark: CopyMark,
    /// The default branch's tip, when it was found.
    pub(crate) tip: Option<String>,
    /// The first confidential paths (`Confidential` only).
    pub(crate) paths: Vec<String>,
    /// Why the measurement did not complete (`Undecided` only).
    pub(crate) error: Option<ForgeError>,
}

impl Measured {
    fn plain(mark: CopyMark) -> Self {
        Self {
            mark,
            tip: None,
            paths: Vec::new(),
            error: None,
        }
    }

    fn undecided(tip: Option<String>, error: ForgeError) -> Self {
        Self {
            mark: CopyMark::Undecided,
            tip,
            paths: Vec::new(),
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone)]
struct Kept {
    tip: String,
    mark: CopyMark,
    paths: Vec<String>,
    at: Instant,
}

/// The kept answers (module docs: cache).
#[derive(Debug, Default)]
pub struct CopyMarks {
    entries: Mutex<HashMap<RepoId, Kept>>,
}

impl CopyMarks {
    fn get(&self, repo: RepoId, tip: &str) -> Option<Measured> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .get(&repo)
            .filter(|k| k.tip == tip)
            .map(|k| Measured {
                mark: k.mark,
                tip: Some(k.tip.clone()), // clone: the answer owns its tip
                paths: k.paths.clone(),   // clone: as above
                error: None,
            })
    }

    /// Keep a decided answer (`Open` / `Confidential` only).
    fn keep(&self, repo: RepoId, measured: &Measured) {
        let (Some(tip), CopyMark::Open | CopyMark::Confidential) = (&measured.tip, measured.mark)
        else {
            return;
        };
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= MAX_MARKS && !entries.contains_key(&repo) {
            let oldest = entries.iter().min_by_key(|(_, k)| k.at).map(|(r, _)| *r);
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        entries.insert(
            repo,
            Kept {
                tip: tip.clone(), // clone: the cache owns its key
                mark: measured.mark,
                paths: measured.paths.clone(), // clone: as above
                at: Instant::now(),
            },
        );
    }

    /// Kept answers now (tests).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// What the default branch's tip of `repo` holds (module docs).
pub(crate) async fn measure(
    smart: &SmartHttp,
    repo: RepoId,
    principal: &str,
    limits: &ConfidentialLimits,
) -> Measured {
    let list = match smart.browse.refs(repo).await {
        Ok(list) => list,
        Err(e) => return Measured::undecided(None, e),
    };
    let unborn = || {
        if list.refs.is_empty() {
            Measured::plain(CopyMark::Open)
        } else {
            Measured::plain(CopyMark::Unborn)
        }
    };
    let Some(branch) = list.default_branch.as_deref() else {
        return unborn();
    };
    let tip = match smart
        .browse
        .resolve_ref(repo, &format!("refs/heads/{branch}"), principal)
        .await
    {
        Ok(tip) => tip.commit,
        Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => return unborn(),
        Err(e) => return Measured::undecided(None, e),
    };
    if let Some(kept) = smart.copy_marks.get(repo, &tip) {
        return kept;
    }
    let measured = match confidential_at(&smart.browse, repo, &tip, limits).await {
        Ok(c) if c.is_empty() => Measured {
            mark: CopyMark::Open,
            tip: Some(tip),
            paths: Vec::new(),
            error: None,
        },
        Ok(c) if c.is_partial() => Measured::undecided(
            Some(tip),
            // A bound left `.md` files unread (rule 3: not known open).
            ForgeError::new(ErrorCode::DocIndexBounded)
                .with_context("operation", "confidential_paths"),
        ),
        Ok(c) => Measured {
            mark: CopyMark::Confidential,
            paths: c
                .first(LOGGED_PATHS)
                .into_iter()
                .map(str::to_owned)
                .collect(),
            tip: Some(tip),
            error: None,
        },
        Err(e) => Measured::undecided(Some(tip), e),
    };
    smart.copy_marks.keep(repo, &measured);
    measured
}

/// The copy decision for `request` (module docs): `Ok` to serve, else the
/// refusal with what the log needs (`FG-AUTH-0022`, or the decision's own
/// error). The caller logs it once, at its boundary.
pub(crate) async fn decide_copy(
    smart: &SmartHttp,
    proof: &Authorized<ProjectCtx>,
    repo: RepoId,
    principal_key: &str,
) -> Result<(), ForgeError> {
    let measured = if proof.principal().is_agent_audience() {
        measure(smart, repo, principal_key, &smart.copy_limits).await
    } else {
        Measured::plain(CopyMark::NotMeasured)
    };
    authorize_repo_copy(proof, measured.mark).map_err(|e| refusal_context(e, repo, &measured))
}

fn refusal_context(mut err: ForgeError, repo: RepoId, measured: &Measured) -> ForgeError {
    err = err.with_context("repo_id", repo.get());
    if let Some(tip) = &measured.tip {
        err = err.with_context("tip", tip.clone()); // clone: the log owns it
    }
    if !measured.paths.is_empty() {
        err = err.with_context("paths", measured.paths.join(","));
    }
    if let Some(cause) = &measured.error {
        err = err.with_context("check_error", cause.code().id());
        for (key, value) in cause.context() {
            let key = match *key {
                "reason" => "check_reason",
                "action" | "repo_id" | "project_id" => continue,
                other => other,
            };
            err = err.with_context(key, value.clone()); // clone: as above
        }
    }
    err
}

#[cfg(test)]
#[path = "repo_copy_tests.rs"]
mod tests;
