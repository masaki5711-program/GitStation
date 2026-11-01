//! What a push does to its refs, asked of git (M1 phase 9 MR4): whether an
//! update is a fast-forward, and which commits it brings.
//!
//! - [`Git::fast_forwards`] — before receive-pack runs, for the updates of
//!   protected branches: `cat-file --batch-check` tells whether the new
//!   object is already in the repository (usually it arrives in the pack,
//!   and then only git can decide — `receive.denyNonFastForwards`,
//!   ADR-0008 option B); for one that is, `rev-list --count
//!   --max-count=1 <old> ^<new>` is 0 exactly when `<old>` is an ancestor
//!   of `<new>`. After the push the same call classifies every applied
//!   update for the record.
//! - [`Git::new_commits`] — after the push, per updated ref: the commits it
//!   brings, newest first, at most [`MAX_LISTED_COMMITS`] kept and all of
//!   them counted (`rev-list`, streamed). Create: commits not reachable
//!   from the refs as they were before the push (`--not --exclude=<every
//!   ref the push updated> --glob=refs/*` plus the old ids); fast-forward / force:
//!   `<new> ^<old>`. One deadline for the whole push: when it passes, the
//!   ref being counted (and every ref not counted yet) is marked partial
//!   and `total_commits` is what was counted (a lower bound,
//!   `PushedRef::total_commits_partial`).
//!
//! Object ids reach git only on stdin, validated (40 / 64 lower-case hex);
//! ref names only as `--exclude=<validated ref name>` (a `RefName` cannot
//! contain glob characters, so the pattern matches that ref only).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use forge_core::push::MAX_LISTED_COMMITS;
use forge_core::{RefName, RepoId};
use forge_errors::{ErrorCode, ForgeError};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::Instant;

use crate::command::Git;
use crate::storage::StorageRoot;
use crate::upload_request::is_object_id;

/// How an update with two non-zero sides relates its objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ancestry {
    /// `old` is an ancestor of `new`.
    FastForward,
    /// History is rewritten (`old` is not an ancestor of `new`).
    NotFastForward,
    /// `new` is not in the repository (yet), or a side is not a commit:
    /// only git can decide, once the pack arrived.
    Unknown,
}

/// One update whose new commits are listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitQuery {
    pub name: RefName,
    /// All zeros for a create.
    pub old: String,
    pub new: String,
}

/// The commits of one update.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitList {
    /// Newest first, at most [`MAX_LISTED_COMMITS`].
    pub commits: Vec<String>,
    /// Every commit counted.
    pub total: u64,
    /// The deadline passed before the count was complete.
    pub partial: bool,
}

fn zero(hex: &str) -> bool {
    hex.bytes().all(|b| b == b'0')
}

fn check_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Result<(), ForgeError> {
    if ids.into_iter().all(is_object_id) {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::Internal).with_context("reason", "push_check_invalid_object_id"))
}

fn timed_out(operation: &'static str, timeout: Duration) -> ForgeError {
    ForgeError::new(ErrorCode::GitTimeout)
        .with_context("operation", operation)
        .with_context("timeout_ms", timeout.as_millis())
}

fn left(deadline: Instant) -> Option<Duration> {
    let rest = deadline.saturating_duration_since(Instant::now());
    (!rest.is_zero()).then_some(rest)
}

impl Git {
    /// For each `(old, new)` (both non-zero): fast-forward or not, or
    /// [`Ancestry::Unknown`] when `new` or `old` is not a commit in the
    /// repository.
    /// `timeout` bounds the whole call (`FG-GIT-0002
    /// operation=fast_forward_check`).
    pub async fn fast_forwards(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        pairs: &[(String, String)],
        timeout: Duration,
    ) -> Result<Vec<Ancestry>, ForgeError> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        check_ids(pairs.iter().flat_map(|(o, n)| [o.as_str(), n.as_str()]))?;
        let deadline = Instant::now() + timeout;
        let repo = storage.existing(id)?;
        let mut news: Vec<&str> = pairs
            .iter()
            .flat_map(|(o, n)| [o.as_str(), n.as_str()])
            .collect();
        news.sort_unstable();
        news.dedup();
        let kinds = self
            .object_types(
                &repo,
                &news,
                left(deadline).ok_or_else(|| timed_out("fast_forward_check", timeout))?,
            )
            .await?;
        let mut out = Vec::with_capacity(pairs.len());
        for (old, new) in pairs {
            let commit = |id: &str| kinds.get(id).map(String::as_str) == Some("commit");
            if !commit(new) || !commit(old) {
                // A new commit still to come in the pack, or an old side
                // that is not a commit here (git checks it against the ref).
                out.push(Ancestry::Unknown);
                continue;
            }
            let rest = left(deadline).ok_or_else(|| timed_out("fast_forward_check", timeout))?;
            let counted = self
                .command("rev-list")
                .arg("--count")
                .arg("--max-count=1")
                .arg("--stdin")
                .repo(&repo)
                .stdin(format!("{old}\n^{new}\n").into_bytes())
                .timeout(rest)
                .output()
                .await?;
            out.push(match counted.stdout_str().trim() {
                "0" => Ancestry::FastForward,
                "1" => Ancestry::NotFastForward,
                _ => {
                    return Err(ForgeError::new(ErrorCode::GitProcessFailed)
                        .with_context("subcommand", "rev-list")
                        .with_context("reason", "unexpected_output"))
                }
            });
        }
        Ok(out)
    }

    /// `object id → type` (`missing` for an absent one).
    async fn object_types(
        &self,
        repo: &Path,
        ids: &[&str],
        timeout: Duration,
    ) -> Result<BTreeMap<String, String>, ForgeError> {
        let mut input = Vec::new();
        for id in ids {
            input.extend_from_slice(id.as_bytes());
            input.push(b'\n');
        }
        let out = self
            .command("cat-file")
            .arg("--batch-check=%(objectname) %(objecttype)")
            .repo(repo)
            .stdin(input)
            .timeout(timeout)
            .output()
            .await?;
        let text = out.stdout_str();
        let mut kinds = BTreeMap::new();
        for line in text.lines() {
            if let Some((id, kind)) = line.split_once(' ') {
                kinds.insert(id.to_owned(), kind.to_owned());
            }
        }
        Ok(kinds)
    }

    /// The new commits of each update (see the module docs); `principal`
    /// is the pusher's stream key, `timeout` the push's whole budget.
    /// Deletes get an empty list. A git failure other than the deadline is
    /// returned (the caller records the push without commits and logs it).
    pub async fn new_commits(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        updates: &[CommitQuery],
        principal: &str,
        timeout: Duration,
    ) -> Result<Vec<CommitList>, ForgeError> {
        check_ids(
            updates
                .iter()
                .flat_map(|u| [u.old.as_str(), u.new.as_str()]),
        )?;
        let repo = storage.existing(id)?;
        let deadline = Instant::now() + timeout;
        let mut out = Vec::with_capacity(updates.len());
        for update in updates {
            if zero(&update.new) {
                out.push(CommitList::default());
                continue;
            }
            let Some(rest) = left(deadline) else {
                out.push(CommitList {
                    partial: true,
                    ..CommitList::default()
                });
                continue;
            };
            out.push(
                self.list_commits(&repo, update, updates, principal, rest)
                    .await?,
            );
        }
        Ok(out)
    }

    async fn list_commits(
        &self,
        repo: &Path,
        update: &CommitQuery,
        all: &[CommitQuery],
        principal: &str,
        budget: Duration,
    ) -> Result<CommitList, ForgeError> {
        let mut stdin = format!("{}\n", update.new);
        let mut cmd = self.command("rev-list").arg("--stdin");
        if zero(&update.old) {
            // The refs before the push: every ref but the updated ones,
            // and the old ids of those.
            cmd = cmd.arg("--not");
            for other in all {
                cmd = cmd.exclude_ref(&other.name);
                if !zero(&other.old) {
                    stdin.push_str(&format!("^{}\n", other.old));
                }
            }
            // `--glob=refs/*`, not `--all`: `--all` adds HEAD, which no
            // `--exclude` removes (it would hide the pushed branch's own
            // commits behind its symbolic HEAD).
            cmd = cmd.arg("--glob=refs/*");
        } else {
            stdin.push_str(&format!("^{}\n", update.old));
        }
        // The stream's own total is a little past the budget, so the
        // deadline below (not git's timeout) decides a slow count.
        let mut stream = cmd
            .repo(repo)
            .timeout(budget + Duration::from_secs(5))
            .spawn_stream(principal)
            .await?;
        let (Some(mut input), Some(stdout)) = (stream.take_stdin(), stream.take_stdout()) else {
            return Err(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "stream_pipes_taken")
            );
        };
        let deadline = Instant::now() + budget;
        let write = async move {
            input.write_all(stdin.as_bytes()).await?;
            input.shutdown().await
        };
        let mut list = CommitList::default();
        let mut lines = BufReader::new(stdout).lines();
        let read = async {
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let id = line.trim();
                        list.total += 1;
                        if list.commits.len() < MAX_LISTED_COMMITS && is_object_id(id) {
                            list.commits.push(id.to_owned());
                        }
                    }
                    Ok(None) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        };
        let joined = async { tokio::join!(write, read) };
        let outcome = tokio::time::timeout_at(deadline, joined).await;
        match outcome {
            Ok((written, read)) => {
                drop(lines);
                // git's own failure first: it explains a write / read error.
                stream.finish().await?;
                for r in [written, read] {
                    r.map_err(|e| {
                        ForgeError::new(ErrorCode::GitProcessFailed)
                            .with_context("subcommand", "rev-list")
                            .with_context("reason", "stream_io_failed")
                            .with_source(e)
                    })?;
                }
                Ok(list)
            }
            Err(_) => {
                drop(lines);
                stream.stop_early().await?;
                list.partial = true;
                Ok(list)
            }
        }
    }
}

#[cfg(test)]
#[path = "push_check_tests.rs"]
mod tests;
