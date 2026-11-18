//! Branch existence for merge request creation (plan 18 S8, ADR-0037 (g)):
//! the question "is there a branch of this name" answered from the
//! repository's branch snapshot ([`crate::refs_cache`]) instead of one
//! `for-each-ref` per asker.
//!
//! - **A hit** (the snapshot has the branch): no git, no browse slot.
//! - **A miss**: answered only from a read that began after the question
//!   — a branch pushed after the snapshot is found — shared by everyone
//!   asking at that moment (one git, one browse slot).
//! - **A failed read** is never "missing" and never "exists": the asker
//!   that ran it gets its error. A failure of that asker's own — no slot
//!   within `repo_wait` (`FG-GIT-0004 reason=repo_browse`), out of time
//!   (`FG-GIT-0002`) — stops there: the askers that waited for the read
//!   are not refused with it. One of them reads, on its own deadline, and
//!   one whose own deadline passes gets its own `FG-GIT-0002
//!   operation=browse`. Any other failure (git not startable, the
//!   repository not on disk) is that failure for every asker waiting.
//! - **More branches than `max_refs`** (or than the output cap): a name
//!   the snapshot does not have is asked of git by its exact name.
//!
//! Nothing here returns a commit id. What depends on one (a merge
//! request's head and versions, mergeability, the merge) reads the live
//! ref: `resolve_ref` / `find_ref`, `forge-review`'s version job.

use forge_core::{RefName, RepoId};
use forge_errors::ForgeError;

use super::refs::RefLookup;
use super::{not_resolved, Browser, Deadline, REFS_OUTPUT_BYTES};
use crate::refs_cache::{BranchAnswer, BranchSnapshot};

/// The `for-each-ref` format of a branch snapshot: the object's type, the
/// type it peels to, the ref name — separated by NUL, one ref per line (a
/// ref name holds neither).
const BRANCH_FORMAT: &str = "--format=%(objecttype)%00%(*objecttype)%00%(refname)";

impl Browser {
    /// `Ok(())` when the branch `name` (a full `refs/heads/…` name) exists
    /// and names a commit; `FG-REPO-0023` (`reason=ref_not_found` /
    /// `not_a_commit`) when git says otherwise; the read's own error when
    /// it could not be asked. See the module docs for what is answered
    /// from the snapshot. A name outside `refs/heads/` is looked up like
    /// [`Browser::find_ref`].
    pub async fn require_branch(&self, repo: RepoId, name: &RefName) -> Result<(), ForgeError> {
        if name.branch().is_none() {
            return match self.find_ref(repo, name).await? {
                RefLookup::Commit(_) => Ok(()),
                RefLookup::Missing => Err(not_resolved("ref_not_found")),
                RefLookup::NotACommit => Err(not_resolved("not_a_commit")),
            };
        }
        let deadline = Deadline::new(self.limits.timeout);
        let lookup = self
            .branches
            .branch(repo, name.as_str(), || self.load_branches(repo, deadline));
        // A caller waiting for another caller's read — or for the next
        // one, after a leader that was refused or ran out of time — waits
        // no longer than its own operation may take.
        let answer = match tokio::time::timeout_at(deadline.at, lookup).await {
            Ok(answer) => answer?,
            Err(_elapsed) => return Err(deadline.expired()),
        };
        match answer {
            BranchAnswer::Commit => Ok(()),
            BranchAnswer::Missing => Err(not_resolved("ref_not_found")),
            BranchAnswer::NotACommit => Err(not_resolved("not_a_commit")),
            BranchAnswer::Unknown => self.require_listed(repo, name, deadline).await,
        }
    }

    /// The repository's branches, read from git: one browse slot, the
    /// caller's deadline.
    async fn load_branches(
        &self,
        repo: RepoId,
        deadline: Deadline,
    ) -> Result<BranchSnapshot, ForgeError> {
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo, deadline).await?;
        let (out, cut) = self
            .git
            .command("for-each-ref")
            .arg(BRANCH_FORMAT)
            .arg("refs/heads/")
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(REFS_OUTPUT_BYTES)
            .await?;
        Ok(BranchSnapshot::parse(
            &out.stdout,
            cut,
            self.limits.max_refs,
        ))
    }

    /// `name` asked of git by its exact name (the snapshot is not
    /// complete and does not have it).
    async fn require_listed(
        &self,
        repo: RepoId,
        name: &RefName,
        deadline: Deadline,
    ) -> Result<(), ForgeError> {
        let names = [name.clone()]; // clone: the query takes a list
        let list = self.query_refs(repo, &names, deadline).await?;
        match list.find(name.as_str()) {
            None => Err(not_resolved("ref_not_found")),
            Some(entry) if entry.commit.is_none() => Err(not_resolved("not_a_commit")),
            Some(_) => Ok(()),
        }
    }
}
