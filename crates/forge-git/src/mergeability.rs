//! Whether a merge request can be merged (M3 R2, ADR-0024 §3): the trial
//! merge with `git merge-tree --write-tree` (no worktree, nothing on disk
//! but objects) and the two `merge-base` questions around it.
//!
//! - [`Git::merge_tree`]: `merge-tree --write-tree --name-only -z
//!   --end-of-options <target> <head>`. Exit 0 is a clean merge
//!   (`<tree>\0`); exit 1 is a conflict (`<tree>\0`, the conflicted paths
//!   each NUL-terminated, an empty record, then git's messages, which are
//!   ignored); anything else is `FG-GIT-0003` (e.g. 128 for unrelated
//!   histories — `--allow-unrelated-histories` is never passed: such a
//!   merge request is "cannot decide", never "clean"). The output is read
//!   with a cap: a clean merge prints one id, so **an output that overflows
//!   the cap is a conflict** whose list is cut (`files_truncated`), never
//!   `FG-GIT-0006` — git is stopped and its exit code is unknown then.
//!   `--merge-base` (git 2.40) is not used: the minimum is 2.38.
//! - [`Git::merge_base`]: `merge-base --end-of-options <a> <b>`; exit 1
//!   (no common ancestor) is `None`.
//! - [`Git::is_ancestor`]: `merge-base --is-ancestor`; exit 1 is `false`.
//!
//! Every call has its caller's timeout (`FG-GIT-0002`) and takes a slot of
//! the global git pool (`FG-GIT-0004`). Revisions are validated object ids
//! after `--end-of-options`; nothing a user typed reaches argv. Each trial
//! merge writes unreferenced trees and blobs. Nothing prunes them yet:
//! forge's repositories run with `gc.auto=0`, and the scheduled maintenance
//! that prunes unreachable objects with a grace period is planned in plan
//! 11 O3 (ADR-0024 §3). Until then they accumulate; runbook
//! `merge-request-mergeability` has the manual `git prune --expire=<grace>`.

use std::path::Path;
use std::time::Duration;

use forge_core::{ObjectId, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use crate::command::{Git, GitCommand};
use crate::revision::Revision;
use crate::storage::StorageRoot;

/// The bounds of one trial merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeTreeLimits {
    pub timeout: Duration,
    /// Bytes of `merge-tree` output read at most (a clean merge prints one
    /// object id; more than this is a conflict whose list is cut).
    pub max_output_bytes: usize,
    /// Conflicted paths kept; more marks the list `files_truncated`.
    pub max_files: usize,
}

impl Default for MergeTreeLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_output_bytes: 256 * 1024,
            max_files: 100,
        }
    }
}

/// The result of a trial merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeTree {
    /// No conflict; the tree the merge would commit.
    Clean { tree: ObjectId },
    /// At least one conflict. `files`: the conflicted paths git listed
    /// (decoded for display; at most `max_files`), `files_truncated` when
    /// there are more or the output overflowed its cap.
    Conflicted {
        files: Vec<String>,
        files_truncated: bool,
    },
}

fn unexpected(subcommand: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed)
        .with_context("subcommand", subcommand)
        .with_context("reason", reason)
}

/// Bytes of `merge-base` output read at most (one object id).
const MERGE_BASE_OUTPUT_BYTES: usize = 4 * 1024;

/// The output of `merge-tree --write-tree --name-only -z` that exited 1
/// (or overflowed: `cut`): the conflicted paths before the empty record.
fn conflicted_files(stdout: &[u8], cut: bool, max_files: usize) -> Result<MergeTree, ForgeError> {
    // Only a record followed by a NUL is complete: the last piece of the
    // split is what came after the final NUL (empty, or a cut name).
    let mut pieces: Vec<&[u8]> = stdout.split(|b| *b == 0).collect();
    pieces.pop();
    let mut records = pieces.into_iter();
    let tree = records.next();
    let tree_ok = tree.is_some_and(|t| ObjectId::parse(&String::from_utf8_lossy(t)).is_ok());
    if !tree_ok && !cut {
        return Err(unexpected("merge-tree", "unexpected_output"));
    }
    let mut files = Vec::new();
    // A cut output is a cut list only if the cut came before the list's end.
    let mut over_max = false;
    let mut ended = false;
    for record in records {
        if record.is_empty() {
            ended = true;
            break;
        }
        if files.len() < max_files {
            files.push(String::from_utf8_lossy(record).into_owned());
        } else {
            over_max = true;
        }
    }
    if !ended && !cut {
        // Exit 1 always ends the list with an empty record.
        return Err(unexpected("merge-tree", "unexpected_output"));
    }
    Ok(MergeTree::Conflicted {
        files,
        files_truncated: over_max || (cut && !ended),
    })
}

/// One object id on the first line / record of `stdout`.
fn one_id(stdout: &[u8], subcommand: &'static str) -> Result<ObjectId, ForgeError> {
    let text = String::from_utf8_lossy(stdout);
    let first = text
        .split(['\0', '\n'])
        .next()
        .ok_or_else(|| unexpected(subcommand, "unexpected_output"))?;
    ObjectId::parse(first.trim()).map_err(|_| unexpected(subcommand, "unexpected_output"))
}

impl Git {
    /// `merge-tree --write-tree --name-only -z --end-of-options <target>
    /// <head>` in `repo` (pinned by `merge_tree_exit_codes_on_git_2_38`).
    pub(crate) fn merge_tree_command(
        &self,
        repo: &Path,
        target: &ObjectId,
        head: &ObjectId,
    ) -> Result<GitCommand, ForgeError> {
        Ok(self
            .command("merge-tree")
            .arg("--write-tree")
            .arg("--name-only")
            .arg("-z")
            .revision(Revision::object_id(target.as_str())?)
            .revision(Revision::object_id(head.as_str())?)
            .repo(repo))
    }

    /// The trial merge of `head` into `target` in repository `id` (module
    /// docs). `FG-GIT-0003` for every exit but 0 and 1 (unrelated
    /// histories, a missing object), `FG-GIT-0002` on timeout.
    pub async fn merge_tree(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        target: &ObjectId,
        head: &ObjectId,
        limits: MergeTreeLimits,
    ) -> Result<MergeTree, ForgeError> {
        let repo = storage.existing(id)?;
        let captured = self
            .merge_tree_command(&repo, target, head)?
            .timeout(limits.timeout)
            .output_accepting(limits.max_output_bytes, &[0, 1])
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        let stdout = &captured.output.stdout;
        match captured.exit_code {
            Some(0) => Ok(MergeTree::Clean {
                tree: one_id(stdout, "merge-tree")?,
            }),
            // Exit 1, or git stopped at the cap: a clean merge never prints
            // more than one id, so an overflow is a conflict.
            Some(1) | None => conflicted_files(stdout, captured.truncated, limits.max_files),
            Some(_) => Err(unexpected("merge-tree", "unaccepted_exit_code")),
        }
    }

    /// The best common ancestor of `a` and `b` (`None`: they share no
    /// history).
    pub async fn merge_base(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        a: &ObjectId,
        b: &ObjectId,
        timeout: Duration,
    ) -> Result<Option<ObjectId>, ForgeError> {
        let repo = storage.existing(id)?;
        let captured = self
            .command("merge-base")
            .revision(Revision::object_id(a.as_str())?)
            .revision(Revision::object_id(b.as_str())?)
            .repo(&repo)
            .timeout(timeout)
            .output_accepting(MERGE_BASE_OUTPUT_BYTES, &[0, 1])
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        match captured.exit_code {
            Some(0) => one_id(&captured.output.stdout, "merge-base").map(Some),
            Some(1) => Ok(None),
            Some(_) | None => Err(unexpected("merge-base", "unexpected_output")),
        }
    }

    /// Whether `ancestor` is an ancestor of (or equal to) `descendant`.
    pub async fn is_ancestor(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        ancestor: &ObjectId,
        descendant: &ObjectId,
        timeout: Duration,
    ) -> Result<bool, ForgeError> {
        let repo = storage.existing(id)?;
        let captured = self
            .command("merge-base")
            .arg("--is-ancestor")
            .revision(Revision::object_id(ancestor.as_str())?)
            .revision(Revision::object_id(descendant.as_str())?)
            .repo(&repo)
            .timeout(timeout)
            .output_accepting(MERGE_BASE_OUTPUT_BYTES, &[0, 1])
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        match captured.exit_code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            Some(_) | None => Err(unexpected("merge-base", "unexpected_output")),
        }
    }
}

#[cfg(test)]
#[path = "mergeability_tests.rs"]
mod tests;
