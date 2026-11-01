//! "Every `want` is reachable from a ref" (ADR-0007 amendment, option (a),
//! M1 phase 9 MR3). git's protocol v2 `fetch` serves any object whose id is
//! known (and v0 any blob), whatever `uploadpack.allowAnySHA1InWant` says;
//! forge therefore checks the wants of every upload-pack request before
//! starting upload-pack, and refuses the whole request when one is not
//! reachable (e.g. a commit or blob removed by a force push).
//!
//! The check, cheapest first (every step a git subprocess, bounded by one
//! deadline; the ids reach git only on stdin, validated as 40 / 64
//! lower-case hex digits, never on argv):
//!
//! 1. `git cat-file --batch-check` classifies each distinct want: missing
//!    objects are unreachable.
//! 2. Commits: `git rev-list --max-count=1 --stdin --not --all` — the wants
//!    (read from stdin before `--not`, so they are the positive side) minus
//!    everything any ref reaches. Empty output = every commit is reachable.
//!    For the usual fetch (wants = ref tips) git only loads the refs.
//! 3. Trees, blobs and tags (partial-clone lazy fetches, annotated tags):
//!    no cheap exact test exists (`rev-list --objects --not --all` does not
//!    mark the trees of the excluded tips, so it reports reachable blobs as
//!    unreachable). forge streams `git rev-list --objects --all
//!    --no-object-names` and looks for the wanted ids: a full object walk,
//!    paid only by requests that want non-commits.
//!
//! `--all` is every ref, which is exactly what upload-pack advertises
//! (forge configures no `transfer.hideRefs` / `uploadpack.hideRefs`; if it
//! ever does, this check must exclude the same refs, e.g. with
//! `--exclude-hidden=uploadpack`, git ≥ 2.42).

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::command::Git;
use crate::storage::StorageRoot;
use crate::upload_request::is_object_id;

/// What the check found. `into_result` turns an unreachable want into the
/// refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WantCheck {
    /// Distinct wanted ids.
    pub wants: usize,
    /// How many were commits (step 2).
    pub commits: usize,
    /// How many were trees, blobs or tags (step 3, the object walk).
    pub objects: usize,
    /// How many no ref reaches (missing ones included).
    pub unreachable: usize,
    /// The first unreachable id (for the log).
    pub first_unreachable: Option<String>,
    pub duration: Duration,
}

impl WantCheck {
    /// `Ok` when every want is reachable, else `FG-GIT-0014`
    /// `reason=want_unreachable` with the counts.
    pub fn into_result(self) -> Result<WantCheck, ForgeError> {
        if self.unreachable == 0 {
            return Ok(self);
        }
        Err(self.context(
            ForgeError::new(ErrorCode::GitProtocolInvalid)
                .with_context("reason", "want_unreachable"),
        ))
    }

    /// The counts as log context.
    pub fn context(&self, err: ForgeError) -> ForgeError {
        let err = err
            .with_context("wants", self.wants)
            .with_context("commits", self.commits)
            .with_context("objects", self.objects)
            .with_context("unreachable", self.unreachable)
            .with_context("check_ms", self.duration.as_millis());
        match &self.first_unreachable {
            Some(oid) => err.with_context("first_unreachable", oid),
            None => err,
        }
    }
}

impl Git {
    /// Check that every id in `wants` is reachable from a ref of repository
    /// `id` (see the module docs). `principal` is the caller's stream key
    /// (step 3 runs as a stream, counted against the caller's cap);
    /// `timeout` bounds the whole check (`FG-GIT-0002` when exceeded).
    /// An id that is not 40 / 64 lower-case hex digits is `FG-GIT-0014
    /// reason=want_invalid` (the parser already refuses them).
    pub async fn check_wants(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        wants: &[String],
        principal: &str,
        timeout: Duration,
    ) -> Result<WantCheck, ForgeError> {
        let started = Instant::now();
        let deadline = started + timeout;
        let distinct: BTreeSet<&str> = wants.iter().map(String::as_str).collect();
        if distinct.iter().any(|w| !is_object_id(w)) {
            return Err(ForgeError::new(ErrorCode::GitProtocolInvalid)
                .with_context("reason", "want_invalid"));
        }
        let mut check = WantCheck {
            wants: distinct.len(),
            commits: 0,
            objects: 0,
            unreachable: 0,
            first_unreachable: None,
            duration: Duration::ZERO,
        };
        if distinct.is_empty() {
            return Ok(check);
        }
        let repo = storage.existing(id)?;
        let mut unreachable: Vec<&str> = Vec::new();
        let mut commits: Vec<&str> = Vec::new();
        let mut objects: BTreeSet<&str> = BTreeSet::new();
        for (want, kind) in self
            .classify(&repo, &distinct, left(deadline, timeout)?)
            .await?
        {
            match kind.as_str() {
                "commit" => commits.push(want),
                "tree" | "blob" | "tag" => {
                    objects.insert(want);
                }
                // "missing", or anything this build does not know.
                _ => unreachable.push(want),
            }
        }
        check.commits = commits.len();
        check.objects = objects.len();
        if !commits.is_empty() {
            // Output means at least one wanted commit is unreachable; the
            // printed id is the first one of the walk (a want, or an
            // unreachable commit between a want and the reachable history).
            if let Some(first) = self
                .first_unreachable_commit(&repo, &commits, left(deadline, timeout)?)
                .await?
            {
                check.unreachable += 1;
                check.first_unreachable = Some(first);
            }
        }
        if !objects.is_empty() {
            let missing = self
                .objects_not_reached(&repo, objects, principal, left(deadline, timeout)?)
                .await?;
            unreachable.extend(missing);
        }
        check.unreachable += unreachable.len();
        if let Some(first) = unreachable.first() {
            check.first_unreachable = Some((*first).to_owned());
        }
        check.duration = started.elapsed();
        Ok(check)
    }

    /// Step 1: `(want, type)` for each distinct want, in input order.
    async fn classify<'w>(
        &self,
        repo: &Path,
        wants: &BTreeSet<&'w str>,
        timeout: Duration,
    ) -> Result<Vec<(&'w str, String)>, ForgeError> {
        let out = self
            .command("cat-file")
            .arg("--batch-check=%(objectname) %(objecttype)")
            .repo(repo)
            .stdin(lines(wants.iter().copied()))
            .timeout(timeout)
            .output()
            .await?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut answers = text.lines();
        let mut kinds = Vec::with_capacity(wants.len());
        for want in wants {
            // `<oid> <type>` or `<input> missing`, one line per input line.
            let kind = answers
                .next()
                .and_then(|line| line.strip_prefix(*want))
                .and_then(|rest| rest.strip_prefix(' '))
                .ok_or_else(|| {
                    ForgeError::new(ErrorCode::GitProcessFailed)
                        .with_context("subcommand", "cat-file")
                        .with_context("reason", "unexpected_output")
                })?;
            kinds.push((*want, kind.to_owned()));
        }
        Ok(kinds)
    }

    /// Step 2: the first commit (in walk order) that a want reaches and no
    /// ref does, or `None` when every wanted commit is reachable.
    async fn first_unreachable_commit(
        &self,
        repo: &Path,
        commits: &[&str],
        timeout: Duration,
    ) -> Result<Option<String>, ForgeError> {
        let out = self
            .command("rev-list")
            .arg("--max-count=1")
            // `--stdin` is read where it stands: before `--not`, the wants
            // are the positive side (pinned by a test).
            .arg("--stdin")
            .arg("--not")
            .arg("--all")
            .repo(repo)
            .stdin(lines(commits.iter().copied()))
            .timeout(timeout)
            .output()
            .await?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_owned))
    }

    /// Step 3: which of `objects` no ref reaches (a full object walk).
    async fn objects_not_reached<'w>(
        &self,
        repo: &Path,
        mut objects: BTreeSet<&'w str>,
        principal: &str,
        timeout: Duration,
    ) -> Result<Vec<&'w str>, ForgeError> {
        let mut stream = self
            .command("rev-list")
            .arg("--objects")
            .arg("--all")
            .arg("--no-object-names")
            .repo(repo)
            .timeout(timeout)
            .spawn_stream(principal)
            .await?;
        // git reads nothing: close its stdin at once.
        drop(stream.take_stdin());
        let Some(stdout) = stream.take_stdout() else {
            return Err(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "stream_stdout_taken")
            );
        };
        let mut reader = BufReader::new(stdout).lines();
        let read = loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    let oid = line.split(' ').next().unwrap_or_default();
                    objects.remove(oid);
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        drop(reader);
        // git's own failure (exit, timeout) first: it explains a read error.
        stream.finish().await?;
        read.map_err(|e| {
            ForgeError::new(ErrorCode::GitProcessFailed)
                .with_context("subcommand", "rev-list")
                .with_context("reason", "stdout_read_failed")
                .with_source(e)
        })?;
        Ok(objects.into_iter().collect())
    }
}

/// The time left before `deadline`, or `FG-GIT-0002` when none is.
fn left(deadline: Instant, timeout: Duration) -> Result<Duration, ForgeError> {
    let rest = deadline.saturating_duration_since(Instant::now());
    if rest.is_zero() {
        return Err(ForgeError::new(ErrorCode::GitTimeout)
            .with_context("operation", "want_check")
            .with_context("timeout_ms", timeout.as_millis()));
    }
    Ok(rest)
}

/// `a\nb\n`.
fn lines<'a>(ids: impl Iterator<Item = &'a str>) -> Vec<u8> {
    let mut out = Vec::new();
    for id in ids {
        out.extend_from_slice(id.as_bytes());
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
#[path = "want_check_tests.rs"]
mod tests;
