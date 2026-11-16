//! The tree of a revert, without a work tree (M4 C10, plan 04 M-4,
//! ADR-0024 Amendment C10): what `git revert` (`-m 1` for a merge commit)
//! would leave on a branch's tip, computed with plumbing only.
//!
//! A merge of a merge request moved its target from `before` (the tip the
//! merge was built on) to `merged` (the merge commit, the squash commit, or
//! the fast-forwarded head). Reverting it is the three-way merge of the
//! target's tip `onto` with `before`, whose base is `merged` — the same
//! merge `git revert` runs (base = the reverted commit, theirs = its first
//! parent). For a squash or a merge commit `before` IS that commit's first
//! parent; for a fast-forward of several commits it is the old tip, so the
//! whole merge is reverted in one commit.
//!
//! The minimum git (2.38) has no `merge-tree --merge-base` (2.40), so the
//! base is made the only merge base by construction: three commits nobody
//! references are written with [`Git::commit_tree`] — `B` (the tree of
//! `merged`, no parent), `O` (the tree of `onto`, parent `B`) and `T` (the
//! tree of `before`, parent `B`) — and [`Git::merge_tree`] merges `O` and
//! `T`. git merges; forge never computes a tree itself. The commits are
//! unreachable like the trial merges' trees (pruned by the scheduled
//! maintenance's grace period, plan 11 O3).
//!
//! - [`Git::tree_of`]: `cat-file --batch-check=%(objectname) %(objecttype)`
//!   with `<commit>^{tree}` on stdin (a validated object id: nothing a user
//!   typed reaches git).
//! - [`Git::revert_tree`]: the merge above; [`RevertTree::Clean`] with the
//!   tree, [`RevertTree::Conflicted`] with the paths git listed, or
//!   [`RevertTree::Unchanged`] when the tree is the tip's own (the change is
//!   not on the tip any more: nothing to revert).
//!
//! Every call has the caller's timeout (`FG-GIT-0002`) and a slot of the
//! global git pool (`FG-GIT-0004`); anything git refuses is `FG-GIT-0003`.

use std::time::Duration;

use forge_core::{ObjectId, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use crate::command::Git;
use crate::commit::{CommitIdentity, NewCommit};
use crate::mergeability::{MergeTree, MergeTreeLimits};
use crate::storage::StorageRoot;

/// The author and committer of the construction commits (never on a
/// branch).
const IDENTITY: (&str, &str, &str) = ("GitStation", "revert-base@gitstation.invalid", "gitstation");

/// `cat-file --batch-check` of one object prints one short line.
const TREE_OUTPUT_BYTES: usize = 1024;

/// The commits of one revert (module docs).
#[derive(Debug, Clone, Copy)]
pub struct RevertInputs<'a> {
    /// The commit the merge moved the target to.
    pub merged: &'a ObjectId,
    /// The target's tip the merge was built on.
    pub before: &'a ObjectId,
    /// The target's tip now: the revert goes on top of it.
    pub onto: &'a ObjectId,
}

/// What reverting a merge on a tip would leave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevertTree {
    /// No conflict; the tree the revert commit has.
    Clean { tree: ObjectId },
    /// The revert conflicts with what the tip changed since (the paths git
    /// listed, at most `max_files`; `files_truncated` when there are more).
    Conflicted {
        files: Vec<String>,
        files_truncated: bool,
    },
    /// The revert would change nothing: the tip no longer holds the change.
    Unchanged,
}

fn unexpected(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed)
        .with_context("subcommand", "cat-file")
        .with_context("reason", reason)
}

impl Git {
    /// The tree of commit `commit` in repository `id`. A missing object or
    /// one that is no commit is `FG-GIT-0003 reason=not_a_commit`.
    pub async fn tree_of(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        commit: &ObjectId,
        timeout: Duration,
    ) -> Result<ObjectId, ForgeError> {
        let repo = storage.existing(id)?;
        let out = self
            .command("cat-file")
            .arg("--batch-check=%(objectname) %(objecttype)")
            .stdin(format!("{}^{{tree}}\n", commit.as_str()).into_bytes())
            .repo(&repo)
            .timeout(timeout)
            .output_accepting(TREE_OUTPUT_BYTES, &[0])
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        let text = String::from_utf8_lossy(&out.output.stdout);
        let line = text.trim();
        match line.split_once(' ') {
            Some((oid, "tree")) => ObjectId::parse(oid)
                .map_err(|_| unexpected("unexpected_output").with_context("repo_id", id.get())),
            _ if line.ends_with(" missing") => Err(unexpected("not_a_commit")
                .with_context("repo_id", id.get())
                .with_context("commit", commit.as_str().to_owned())),
            _ => Err(unexpected("unexpected_output").with_context("repo_id", id.get())),
        }
    }

    /// The tree of reverting `inputs.merged` (built on `inputs.before`) on
    /// `inputs.onto` in repository `id` (module docs). `limits.timeout`
    /// bounds every git call of it.
    pub async fn revert_tree(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        inputs: RevertInputs<'_>,
        limits: MergeTreeLimits,
    ) -> Result<RevertTree, ForgeError> {
        let t = limits.timeout;
        let merged_tree = self.tree_of(storage, id, inputs.merged, t).await?;
        let before_tree = self.tree_of(storage, id, inputs.before, t).await?;
        let onto_tree = self.tree_of(storage, id, inputs.onto, t).await?;
        let (name, email, username) = IDENTITY;
        let identity = CommitIdentity::new(name, email, username)?;
        let write = |tree: ObjectId, parents: Vec<ObjectId>, message: &'static str| {
            let identity = &identity;
            async move {
                self.commit_tree(
                    storage,
                    id,
                    &NewCommit {
                        tree: &tree,
                        parents: &parents,
                        message,
                        author: identity,
                        committer: identity,
                    },
                    t,
                )
                .await
            }
        };
        let base = write(merged_tree, Vec::new(), "revert base\n").await?;
        let ours = write(onto_tree.clone(), vec![base.clone()], "revert onto\n").await?; // clone: the tip's tree is compared below; the base is the other side's parent too
        let theirs = write(before_tree, vec![base], "revert before\n").await?;
        match self.merge_tree(storage, id, &ours, &theirs, limits).await? {
            MergeTree::Clean { tree } if tree == onto_tree => Ok(RevertTree::Unchanged),
            MergeTree::Clean { tree } => Ok(RevertTree::Clean { tree }),
            MergeTree::Conflicted {
                files,
                files_truncated,
            } => Ok(RevertTree::Conflicted {
                files,
                files_truncated,
            }),
        }
    }
}

#[cfg(test)]
#[path = "revert_tests.rs"]
mod tests;
