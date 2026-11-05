//! The git facts approvals and CODEOWNERS are decided on (M3 R5, ADR-0024
//! §8.3, §9). Each call is bounded by its caller's timeout and an output
//! cap; nothing here decides what a fact means:
//!
//! - [`Git::changed_paths`]: `diff-tree --no-renames -r -z --name-only
//!   --end-of-options <base> <head>` — every changed path, renames counted
//!   as a deletion and an addition (both owners are needed). Never the
//!   display diff (`browse::diff`: limits, rename detection). An output
//!   past the cap or more than `max_paths` paths is
//!   [`ChangedPaths::TooMany`]; a path that is not UTF-8 is
//!   [`ChangedPaths::NonUtf8`] (a glob cannot be matched against it: the
//!   caller fails closed).
//! - [`Git::range_commits`]: `rev-list --parents -n <cap+1> --end-of-options
//!   <head> [^<base>]` — the commits of the range, or `None` when there are
//!   more than the cap.
//! - [`Git::codeowners_file`]: the CODEOWNERS file of a commit
//!   (`.forge/CODEOWNERS`, else `CODEOWNERS`) through `ls-tree -z` and
//!   `cat-file blob` with a size cap.

use std::path::Path;
use std::time::Duration;

use forge_core::{ObjectId, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use crate::command::Git;
use crate::revision::Revision;
use crate::storage::StorageRoot;

/// The CODEOWNERS locations, first found wins (ADR-0024 §9).
pub const CODEOWNERS_PATHS: [&str; 2] = [".forge/CODEOWNERS", "CODEOWNERS"];

/// The changed paths between two commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangedPaths {
    /// Every changed path (sorted as git prints them).
    Complete(Vec<String>),
    /// More paths than the cap (or more output than its byte cap).
    TooMany,
    /// A changed path is not UTF-8.
    NonUtf8,
}

/// The commits of a range and its bottoms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRange {
    /// Newest first.
    pub commits: Vec<ObjectId>,
    /// The commits none of whose parents is in the range.
    pub bottoms: Vec<ObjectId>,
}

/// A CODEOWNERS file as read from a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeownersFile {
    /// Neither location holds an entry.
    Absent,
    /// The file (`path`: which location) and its bytes.
    Found { path: &'static str, bytes: Vec<u8> },
    /// The entry is larger than `max_bytes`.
    TooLarge { path: &'static str },
    /// The entry is not a regular file (a directory, a symlink, a
    /// submodule).
    NotAFile { path: &'static str },
}

fn unexpected(subcommand: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed)
        .with_context("subcommand", subcommand)
        .with_context("reason", reason)
}

/// The NUL-terminated records of `stdout` (a cut last record is dropped
/// by the caller through `truncated`).
fn records(stdout: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pieces: Vec<&[u8]> = stdout.split(|b| *b == 0).collect();
    // What follows the final NUL: empty for a complete output.
    pieces.pop();
    pieces.into_iter()
}

impl Git {
    fn repo_dir(storage: &StorageRoot, id: RepoId) -> Result<std::path::PathBuf, ForgeError> {
        storage.existing(id)
    }

    /// Every path changed between `base` and `head` (module docs).
    pub async fn changed_paths(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        base: &ObjectId,
        head: &ObjectId,
        max_paths: usize,
        timeout: Duration,
    ) -> Result<ChangedPaths, ForgeError> {
        self.diff_tree_paths(storage, id, (base, head), None, max_paths, timeout)
            .await
    }

    /// The paths deleted between `base` and `head` (`--diff-filter=D`,
    /// renames counted as a deletion; M3 X6: the conflict scan's directory
    /// renames), as [`Git::changed_paths`].
    pub async fn deleted_paths(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        base: &ObjectId,
        head: &ObjectId,
        max_paths: usize,
        timeout: Duration,
    ) -> Result<ChangedPaths, ForgeError> {
        self.diff_tree_paths(
            storage,
            id,
            (base, head),
            Some("--diff-filter=D"),
            max_paths,
            timeout,
        )
        .await
    }

    async fn diff_tree_paths(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        (base, head): (&ObjectId, &ObjectId),
        filter: Option<&'static str>,
        max_paths: usize,
        timeout: Duration,
    ) -> Result<ChangedPaths, ForgeError> {
        let repo = Self::repo_dir(storage, id)?;
        // 4 KiB per path at most, plus the NUL.
        let cap = max_paths.saturating_mul(4097).max(4096);
        let mut command = self
            .command("diff-tree")
            .arg("--no-renames")
            .arg("-r")
            .arg("-z")
            .arg("--name-only");
        if let Some(filter) = filter {
            command = command.arg(filter);
        }
        let (output, truncated) = command
            .revision(Revision::object_id(base.as_str())?)
            .revision(Revision::object_id(head.as_str())?)
            .repo(&repo)
            .timeout(timeout)
            .output_truncated(cap)
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        if truncated {
            return Ok(ChangedPaths::TooMany);
        }
        let mut paths = Vec::new();
        for record in records(&output.stdout) {
            if paths.len() >= max_paths {
                return Ok(ChangedPaths::TooMany);
            }
            match std::str::from_utf8(record) {
                Ok(p) => paths.push(p.to_owned()),
                Err(_) => return Ok(ChangedPaths::NonUtf8),
            }
        }
        Ok(ChangedPaths::Complete(paths))
    }

    /// The commits reachable from `head` and not from `base` (`None`: the
    /// whole history of `head`), newest first, with the range's bottoms
    /// (its commits none of whose parents is in the range: a commit reaches
    /// the range exactly when it reaches a bottom); `None` when there are
    /// more than `cap`.
    pub async fn range_commits(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        base: Option<&ObjectId>,
        head: &ObjectId,
        cap: usize,
        timeout: Duration,
    ) -> Result<Option<CommitRange>, ForgeError> {
        let repo = Self::repo_dir(storage, id)?;
        let mut command = self
            .command("rev-list")
            .arg("--parents")
            .arg("-n")
            .arg_owned(cap.saturating_add(1).to_string())
            .revision(Revision::object_id(head.as_str())?);
        if let Some(base) = base {
            command = command.revision(Revision::object_id(base.as_str())?.excluding());
        }
        let output = command
            .repo(&repo)
            .timeout(timeout)
            .output()
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        let mut rows: Vec<(ObjectId, Vec<ObjectId>)> = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut ids = line.split_ascii_whitespace().map(|raw| {
                ObjectId::parse(raw).map_err(|_| unexpected("rev-list", "unexpected_output"))
            });
            let Some(commit) = ids.next() else {
                continue;
            };
            let parents = ids.collect::<Result<Vec<_>, _>>()?;
            rows.push((commit?, parents));
        }
        if rows.len() > cap {
            return Ok(None);
        }
        let set: std::collections::BTreeSet<&ObjectId> = rows.iter().map(|(c, _)| c).collect();
        let bottoms = rows
            .iter()
            .filter(|(_, parents)| !parents.iter().any(|p| set.contains(p)))
            .map(|(c, _)| c.clone()) // clone: the range owns its ids
            .collect();
        Ok(Some(CommitRange {
            commits: rows.into_iter().map(|(c, _)| c).collect(),
            bottoms,
        }))
    }

    /// The CODEOWNERS file of `commit` (module docs), at most `max_bytes`.
    pub async fn codeowners_file(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        commit: &ObjectId,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<CodeownersFile, ForgeError> {
        let repo = Self::repo_dir(storage, id)?;
        let listed = self
            .command("ls-tree")
            .arg("-z")
            .revision(Revision::object_id(commit.as_str())?)
            .path(CODEOWNERS_PATHS[0])
            .path(CODEOWNERS_PATHS[1])
            .repo(&repo)
            .timeout(timeout)
            .output()
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        let entries = parse_ls_tree(&listed.stdout)?;
        for path in CODEOWNERS_PATHS {
            let Some((mode, kind, oid)) = entries
                .iter()
                .find(|(_, _, _, p)| p == path)
                .map(|(mode, kind, oid, _)| (mode.as_str(), kind.as_str(), oid))
            else {
                continue;
            };
            if kind != "blob" || !matches!(mode, "100644" | "100755") {
                return Ok(CodeownersFile::NotAFile { path });
            }
            return self
                .blob_capped(&repo, id, oid, path, max_bytes, timeout)
                .await;
        }
        Ok(CodeownersFile::Absent)
    }

    async fn blob_capped(
        &self,
        repo: &Path,
        id: RepoId,
        oid: &ObjectId,
        path: &'static str,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<CodeownersFile, ForgeError> {
        let (output, truncated) = self
            .command("cat-file")
            .arg("blob")
            .revision(Revision::object_id(oid.as_str())?)
            .repo(repo)
            .timeout(timeout)
            .output_truncated(max_bytes.saturating_add(1))
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        if truncated || output.stdout.len() > max_bytes {
            return Ok(CodeownersFile::TooLarge { path });
        }
        Ok(CodeownersFile::Found {
            path,
            bytes: output.stdout,
        })
    }
}

/// `ls-tree -z` records: `<mode> SP <type> SP <oid> TAB <path>`.
fn parse_ls_tree(stdout: &[u8]) -> Result<Vec<(String, String, ObjectId, String)>, ForgeError> {
    let mut out = Vec::new();
    for record in records(stdout) {
        let text = String::from_utf8_lossy(record);
        let (meta, path) = text
            .split_once('\t')
            .ok_or_else(|| unexpected("ls-tree", "unexpected_output"))?;
        let mut parts = meta.split(' ');
        let (Some(mode), Some(kind), Some(oid)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(unexpected("ls-tree", "unexpected_output"));
        };
        let oid = ObjectId::parse(oid).map_err(|_| unexpected("ls-tree", "unexpected_output"))?;
        out.push((mode.to_owned(), kind.to_owned(), oid, path.to_owned()));
    }
    Ok(out)
}

#[cfg(test)]
#[path = "review_inputs_tests.rs"]
mod tests;
