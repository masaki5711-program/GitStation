//! Removing a repository's bare directory (step D5 MR4, the purge): the
//! path is [`StorageRoot::path_for`] of the repository id — never a path
//! from a request — and every directory from the root down to the shard
//! must be a real directory (no symlink, no file), as for creation.
//!
//! First every long-lived `cat-file` process this process runs for the
//! repository is closed and its cached objects dropped
//! ([`crate::browse::release_repository`]): on Windows a directory holding
//! a file another process has open can be neither renamed nor deleted. A
//! `forge serve` process elsewhere closes its own after
//! `BrowseLimits::batch_idle` (60 s) unused; a removal it blocks fails with
//! `FG-REPO-0003` and the purge's next run resumes it.
//!
//! The removal is idempotent and resumable, so the purge can run it again
//! after a crash or a timeout: the repository directory is first renamed
//! aside to `.purge-<id>-<uuid>.git` in its shard (one atomic rename: the
//! path is free from then on), then the shard is swept of every leftover of
//! this repository — directories named `.purge-<id>-*` (an earlier removal
//! that was cut off) and `.tmp-<id>-*` (a provisioning attempt that crashed
//! before its rename) — and each is deleted (`remove_dir_all`, which never
//! follows a symlink inside the tree). Nothing at the path and no leftover
//! is success ([`Removal::existed`] = false).
//!
//! A symlink or a file where the repository or a leftover should be is
//! refused (`FG-REPO-0001`, `reason=symlink` / `not_dir`) and left alone:
//! forge never writes one there, so it is tampering for an operator to
//! look at, never something to delete blindly.
//!
//! Every step runs under the caller's deadline (blocking steps on the
//! blocking pool): an overrun is `FG-REPO-0003` with `operation` and
//! `reason=deadline`. A deletion cut off by the deadline may go on in the
//! background; the next call sweeps what is left.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use uuid::Uuid;

use super::{io_failed, rejected, Budget, StorageRoot};

/// How long a removal blocked by a file in use waits before its one
/// retry: a `forge serve` process closes an idle batch process after
/// `BrowseLimits::batch_idle` (60 s), its sweep runs every 30 s.
pub const FILES_IN_USE_RETRY: Duration = Duration::from_secs(95);

/// An I/O failure because another process holds a file or directory open
/// (Windows `ERROR_SHARING_VIOLATION` 32 / `ERROR_LOCK_VIOLATION` 33).
fn files_in_use(e: &ForgeError) -> bool {
    cfg!(windows)
        && e.code() == ErrorCode::StorageIoFailed
        && std::error::Error::source(e)
            .and_then(|s| s.downcast_ref::<io::Error>())
            .is_some_and(|io| matches!(io.raw_os_error(), Some(32 | 33)))
}

/// What [`StorageRoot::remove_until`] found and removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "existed says whether a directory was there; the purge records it"]
pub struct Removal {
    /// The repository directory was at its path (and is gone now).
    pub existed: bool,
    /// Leftover directories of this repository removed from the shard
    /// (an interrupted removal or provisioning attempt), the renamed
    /// repository directory included.
    pub swept: usize,
}

/// The name prefixes of this repository's leftovers in its shard.
fn leftover_prefixes(id: RepoId) -> [String; 2] {
    [
        format!(".purge-{}-", id.get()),
        format!(".tmp-{}-", id.get()),
    ]
}

impl StorageRoot {
    /// Remove the bare repository of `id` and its leftovers (module docs),
    /// every step before `deadline`. Errors: `FG-REPO-0001` (a symlink or a
    /// file at the path, in the chain to it, or as a leftover), `FG-REPO-0003`
    /// (an I/O failure; `reason=deadline` when the deadline passed). A
    /// missing directory, shard or `repos/` is not an error.
    pub async fn remove_until(&self, id: RepoId, deadline: Instant) -> Result<Removal, ForgeError> {
        let budget = Budget(Some(deadline));
        // This process's batch processes hold the repository's files open
        // (Windows cannot rename or delete those): closed first, bounded.
        crate::browse::release_repository_async(self, id).await;
        let path = self.path_for(id);
        let Some(shard) = path.parent().map(Path::to_path_buf) else {
            return Err(rejected("outside_root").with_context("repo_id", id));
        };
        // The chain down to the shard: real directories, or absent.
        let root = self.clone(); // clone: owned by the blocking step
        let inspected = shard.clone(); // clone: owned by the blocking step
        let present = budget
            .blocking("inspect", id, move || root.shard_present(&inspected, id))
            .await?;
        if !present {
            return Ok(Removal {
                existed: false,
                swept: 0,
            });
        }
        let existed = self.set_aside(id, &path, &shard, budget).await?;
        let swept = budget
            .blocking("remove", id, move || sweep(&shard, id))
            .await?;
        Ok(Removal { existed, swept })
    }

    /// [`StorageRoot::remove_until`] without a deadline, blocking (run it on
    /// the blocking pool): the purge (D5 MR4) runs it in a task that keeps
    /// the project's purge lock until it really ends, so a removal slower
    /// than the purge's wait is never started twice on the same tree.
    ///
    /// A file in use (Windows os error 32 / 33: another process — a git a
    /// `forge serve` still runs there — holds the directory or a file in
    /// it) is retried once after [`FILES_IN_USE_RETRY`], by when every
    /// idle batch process of `forge serve` has closed; still in use, it is
    /// `FG-REPO-0003` with `reason=files_in_use` (the purge logs it once,
    /// as FG-REPO-0020, and its next run resumes).
    pub fn remove_blocking(&self, id: RepoId) -> Result<Removal, ForgeError> {
        self.remove_blocking_retrying(id, FILES_IN_USE_RETRY)
    }

    /// [`StorageRoot::remove_blocking`] with another wait before the retry
    /// (tests).
    pub(crate) fn remove_blocking_retrying(
        &self,
        id: RepoId,
        retry_after: Duration,
    ) -> Result<Removal, ForgeError> {
        // As in `remove_until`: no open file of this process in the way.
        crate::browse::release_repository(self, id);
        match self.remove_once(id) {
            Err(e) if files_in_use(&e) => {
                tracing::info!(
                    target: "forge_git::storage",
                    event = "forge.repo.remove_in_use_retry",
                    repo_id = id.get(),
                    retry_after_s = retry_after.as_secs(),
                    "a file of the repository is in use; the removal is retried once"
                );
                std::thread::sleep(retry_after);
                crate::browse::release_repository(self, id);
                self.remove_once(id).map_err(|e| {
                    if files_in_use(&e) {
                        e.with_context("reason", "files_in_use")
                            .with_context("retried_after_s", retry_after.as_secs())
                    } else {
                        e
                    }
                })
            }
            other => other,
        }
    }

    fn remove_once(&self, id: RepoId) -> Result<Removal, ForgeError> {
        let path = self.path_for(id);
        let Some(shard) = path.parent().map(Path::to_path_buf) else {
            return Err(rejected("outside_root").with_context("repo_id", id));
        };
        if !self.shard_present(&shard, id)? {
            return Ok(Removal {
                existed: false,
                swept: 0,
            });
        }
        let existed = match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(io_failed("inspect", e).with_context("repo_id", id)),
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(rejected("symlink").with_context("repo_id", id))
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(rejected("not_dir").with_context("repo_id", id))
            }
            Ok(_) => match std::fs::rename(&path, aside(&shard, id)) {
                Ok(()) => true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                Err(e) => return Err(io_failed("rename", e).with_context("repo_id", id)),
            },
        };
        let swept = sweep(&shard, id)?;
        Ok(Removal { existed, swept })
    }

    /// Whether the shard directory exists, checked as a real directory under
    /// the root with no symlink on the way (`FG-REPO-0001` otherwise).
    fn shard_present(&self, shard: &Path, id: RepoId) -> Result<bool, ForgeError> {
        match self.check_chain(shard, id) {
            Ok(()) => {}
            Err(e) if e.code() == ErrorCode::StorageMissing => return Ok(false),
            Err(e) => return Err(e),
        }
        self.ensure_inside(shard, id)?;
        Ok(true)
    }

    /// Rename the repository directory to a fresh `.purge-<id>-<uuid>.git`
    /// in its shard; `false` when nothing is at the path.
    async fn set_aside(
        &self,
        id: RepoId,
        path: &Path,
        shard: &Path,
        budget: Budget,
    ) -> Result<bool, ForgeError> {
        let Some(meta) = self.occupant(path, id, budget).await? else {
            return Ok(false);
        };
        if meta.file_type().is_symlink() {
            return Err(rejected("symlink").with_context("repo_id", id));
        }
        if !meta.is_dir() {
            return Err(rejected("not_dir").with_context("repo_id", id));
        }
        let aside = aside(shard, id);
        budget
            .fs("rename", id, async {
                match tokio::fs::rename(path, &aside).await {
                    Ok(()) => Ok(true),
                    // A concurrent removal renamed it first: its leftover is swept.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                    Err(e) => Err(io_failed("rename", e).with_context("repo_id", id)),
                }
            })
            .await
    }
}

/// A fresh name to set the repository directory of `id` aside to.
fn aside(shard: &Path, id: RepoId) -> PathBuf {
    shard.join(format!(
        ".purge-{}-{}.git",
        id.get(),
        Uuid::new_v4().simple()
    ))
}

/// Delete every leftover directory of `id` in `shard` (blocking); returns
/// how many were removed. A symlink or a file with a leftover's name is
/// refused and left in place.
fn sweep(shard: &Path, id: RepoId) -> Result<usize, ForgeError> {
    let prefixes = leftover_prefixes(id);
    let entries = match std::fs::read_dir(shard) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(io_failed("inspect", e).with_context("repo_id", id)),
    };
    let mut leftovers = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_failed("inspect", e).with_context("repo_id", id))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if prefixes.iter().any(|p| name.starts_with(p.as_str())) {
            leftovers.push(entry.path());
        }
    }
    let mut swept = 0;
    for leftover in leftovers {
        let meta = match std::fs::symlink_metadata(&leftover) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_failed("inspect", e).with_context("repo_id", id)),
        };
        if meta.file_type().is_symlink() {
            return Err(rejected("symlink")
                .with_context("repo_id", id)
                .with_context("leftover", leftover.display()));
        }
        if !meta.is_dir() {
            return Err(rejected("not_dir")
                .with_context("repo_id", id)
                .with_context("leftover", leftover.display()));
        }
        match std::fs::remove_dir_all(&leftover) {
            Ok(()) => swept += 1,
            // A removal cut off earlier finished it in the meantime.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(io_failed("remove", e)
                    .with_context("repo_id", id)
                    .with_context("leftover", leftover.display()))
            }
        }
    }
    Ok(swept)
}
