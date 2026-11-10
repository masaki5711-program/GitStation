//! The merge result of a merge request (M4 C7, plan 04 M-1, ADR-0024
//! Amendment 2026-11-11): the commit that merges the head into the target
//! tip, and the hidden ref `refs/merge-requests/<iid>/merge` that keeps it.
//!
//! - [`Git::write_merge_result`]: `commit-tree` (the argv of
//!   [`crate::commit`]: hooks pinned to the empty directory, parents and the
//!   tree as validated object ids after `--end-of-options`, the message on
//!   standard input) with the tree the trial merge (`merge-tree
//!   --write-tree`) found clean and the parents `<target tip>` then
//!   `<head>`. git merges; forge never computes a tree itself.
//! - [`Git::set_merge_result_ref`]: point the ref at a result, or delete it
//!   (a conflict leaves no merge ref), as ONE compare-and-swap
//!   `update-ref` transaction on the value read just before. A lost swap is
//!   read again: already at the wanted value is success, anything else is
//!   the `FG-GIT-0003` of the refused transaction (the job retries).
//! - [`Git::merge_result_exists`]: `cat-file -e` of a stored merge result
//!   before it is reused (`gc` may have pruned it once the ref moved on).
//!
//! The ref is under `refs/merge-requests/`, so Smart HTTP never advertises
//! it, receive-pack refuses to write it (FG-GIT-0010 first) and the want
//! check leaves it out (`hidden_refs`). A CI job fetches its pinned merge
//! result through its own pipeline ref (`refs/pipelines/<id>/head`).

use std::time::Duration;

use forge_core::{ObjectId, RefName, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use crate::command::Git;
use crate::commit::{CommitIdentity, NewCommit};
use crate::refs::RefChange;
use crate::revision::Revision;
use crate::storage::StorageRoot;

/// The author and committer of every merge result (not a person: the
/// merge result is forge's trial merge, never pushed to a branch).
const IDENTITY: (&str, &str, &str) = (
    "GitStation",
    "merge-result@gitstation.invalid",
    "gitstation",
);

/// `cat-file -e` prints nothing on success; its output is capped tight.
const EXISTS_OUTPUT_BYTES: usize = 1024;

/// What [`Git::set_merge_result_ref`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeRefChange {
    /// The ref already had the wanted value (or was absent and should be).
    Unchanged,
    Created,
    Moved {
        from: ObjectId,
    },
    Deleted {
        from: ObjectId,
    },
}

impl MergeRefChange {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Created => "created",
            Self::Moved { .. } => "moved",
            Self::Deleted { .. } => "deleted",
        }
    }
}

/// The commit message of a merge result.
fn message(iid: i64, target: &ObjectId, head: &ObjectId) -> String {
    format!(
        "Merge result of merge request !{iid}\n\nhead {}\ntarget {}\n",
        head.as_str(),
        target.as_str()
    )
}

impl Git {
    /// Write the merge commit of `head` into `target` with `tree` (the
    /// clean trial merge's) in repository `id`; its object id.
    /// `FG-GIT-0002` on timeout, `FG-GIT-0003` when git refuses.
    pub async fn write_merge_result(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        iid: i64,
        (tree, target, head): (&ObjectId, &ObjectId, &ObjectId),
        timeout: Duration,
    ) -> Result<ObjectId, ForgeError> {
        let (name, email, username) = IDENTITY;
        let identity = CommitIdentity::new(name, email, username)?;
        let parents = [target.clone(), head.clone()]; // clone: the commit owns its parents
        let text = message(iid, target, head);
        self.commit_tree(
            storage,
            id,
            &NewCommit {
                tree,
                parents: &parents,
                message: &text,
                author: &identity,
                committer: &identity,
            },
            timeout,
        )
        .await
    }

    /// Point `refs/merge-requests/<iid>/merge` at `to`, or delete it for
    /// `None` (module docs).
    pub async fn set_merge_result_ref(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        iid: i64,
        to: Option<&ObjectId>,
        timeout: Duration,
    ) -> Result<MergeRefChange, ForgeError> {
        let name = crate::hidden_refs::merge_request_merge_ref(iid)?;
        let current = self.read_ref(storage, id, &name, timeout).await?;
        let (change, done) = match (current, to) {
            (Some(at), Some(to)) if at == *to => return Ok(MergeRefChange::Unchanged),
            (None, None) => return Ok(MergeRefChange::Unchanged),
            (None, Some(to)) => (
                RefChange::Create {
                    name: name.clone(), // clone: the name is read again on a lost swap
                    new: to.clone(),    // clone: the change owns its id
                },
                MergeRefChange::Created,
            ),
            (Some(at), Some(to)) => (
                RefChange::Update {
                    name: name.clone(), // clone: as above
                    new: to.clone(),    // clone: as above
                    old: at.clone(),    // clone: the answer keeps it
                },
                MergeRefChange::Moved { from: at },
            ),
            (Some(at), None) => (
                RefChange::Delete {
                    name: name.clone(), // clone: as above
                    old: at.clone(),    // clone: as above
                },
                MergeRefChange::Deleted { from: at },
            ),
        };
        match self.update_refs(storage, id, &[change], timeout).await {
            Ok(_) => Ok(done),
            Err(e) => {
                let now = self.read_ref(storage, id, &name, timeout).await?;
                if now.as_ref() == to {
                    return Ok(MergeRefChange::Unchanged);
                }
                Err(e
                    .with_context("ref", name.as_str().to_owned())
                    .with_context("iid", iid))
            }
        }
    }

    /// Whether the commit `result` is in repository `id` (`cat-file -e`:
    /// exit 0 present, 1 absent; the id after `--end-of-options`). A merge
    /// result stored for a pair is checked before it is reused: once the
    /// ref moved on, `gc` may have pruned it (review MEDIUM).
    /// `FG-GIT-0002` on timeout, `FG-GIT-0003` for any other exit.
    pub async fn merge_result_exists(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        result: &ObjectId,
        timeout: Duration,
    ) -> Result<bool, ForgeError> {
        let repo = storage.existing(id)?;
        let captured = self
            .command("cat-file")
            .arg("-e")
            .revision(Revision::object_id(result.as_str())?)
            .repo(&repo)
            .timeout(timeout)
            .output_accepting(EXISTS_OUTPUT_BYTES, &[0, 1])
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        match captured.exit_code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            Some(_) | None => Err(ForgeError::new(ErrorCode::GitProcessFailed)
                .with_context("subcommand", "cat-file")
                .with_context("reason", "unexpected_output")
                .with_context("repo_id", id.get())),
        }
    }

    /// The object `refs/merge-requests/<iid>/merge` points to (`None`:
    /// absent).
    pub async fn merge_result_ref(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        iid: i64,
        timeout: Duration,
    ) -> Result<Option<ObjectId>, ForgeError> {
        let name: RefName = crate::hidden_refs::merge_request_merge_ref(iid)?;
        self.read_ref(storage, id, &name, timeout).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use super::*;

    #[test]
    fn the_identity_and_message_are_valid_commit_input() {
        let (name, email, username) = IDENTITY;
        let identity = CommitIdentity::new(name, email, username).expect("identity");
        assert!(!identity.fell_back());
        let a = ObjectId::parse(&"a".repeat(40)).expect("oid");
        let b = ObjectId::parse(&"b".repeat(40)).expect("oid");
        let text = message(3, &a, &b);
        assert!(text.starts_with("Merge result of merge request !3\n"));
        assert!(text.contains(&format!("head {}", b.as_str())));
        assert!(text.contains(&format!("target {}", a.as_str())));
    }
}
