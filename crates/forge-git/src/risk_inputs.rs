//! What the deterministic risk rules read from git (M3 X7, plan 04 M-6,
//! ADR-0023 Amendment X7): the change between a merge request's merge base
//! and its head with renames paired ([`Git::risk_diff`]), and the size of
//! the target's tree ([`Git::tree_totals`]) for the mass-deletion share.
//! Nothing here decides what a change means (that is `forge_core::risk`).
//!
//! Both are one `diff-tree` each, bounded by the caller's timeout and an
//! output cap; an argument vector, revisions as object ids after
//! `--end-of-options` ([`Revision`]), the repository from [`StorageRoot`].
//! Unlike `Git::changed_entries` (the approval carry, `--no-renames`),
//! `risk_diff` runs with `-M` so a rename is not a deletion plus an
//! addition.

use std::collections::HashMap;
use std::time::Duration;

use forge_core::risk::{ChangeStatus, ChangedFile, TreeTotals};
use forge_core::{ObjectId, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use crate::command::Git;
use crate::revision::Revision;
use crate::storage::StorageRoot;

/// The change between two commits, as the risk rules need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RiskDiff {
    Complete(Vec<ChangedFile>),
    /// More entries than the cap (or more output than its byte cap).
    TooMany,
    /// A changed path is not UTF-8.
    NonUtf8,
}

fn unexpected(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed)
        .with_context("subcommand", "diff-tree")
        .with_context("reason", reason)
}

fn mode(raw: &str) -> Result<u32, ForgeError> {
    u32::from_str_radix(raw, 8).map_err(|_| unexpected("unexpected_output"))
}

fn status(raw: &str) -> Result<ChangeStatus, ForgeError> {
    match raw.chars().next() {
        Some('A') => Ok(ChangeStatus::Added),
        Some('M') => Ok(ChangeStatus::Modified),
        Some('D') => Ok(ChangeStatus::Deleted),
        Some('T') => Ok(ChangeStatus::TypeChanged),
        Some('R') => Ok(ChangeStatus::Renamed),
        _ => Err(unexpected("unexpected_status")),
    }
}

/// Parse `--raw --numstat -z` output with renames: every raw record
/// (`:<old mode> <new mode> <old id> <new id> <status>` NUL path NUL, a
/// rename with the old and the new path) first, then every numstat record
/// (`<added>\t<deleted>\t<path>` NUL; for a rename the path is empty and the
/// old and new paths follow as NUL-terminated pieces; `-` for binary).
pub(crate) fn parse_risk_diff(stdout: &[u8], max: usize) -> Result<RiskDiff, ForgeError> {
    let mut pieces: Vec<&[u8]> = stdout.split(|b| *b == 0).collect();
    pieces.pop(); // what follows the last NUL: empty for a complete output
    let mut files: Vec<ChangedFile> = Vec::new();
    let mut by_path: HashMap<String, usize> = HashMap::new();
    let mut it = pieces.into_iter();
    while let Some(record) = it.next() {
        let Ok(text) = std::str::from_utf8(record) else {
            return Ok(RiskDiff::NonUtf8);
        };
        if let Some(header) = text.strip_prefix(':') {
            let mut fields = header.split(' ');
            let old_mode = mode(fields.next().unwrap_or(""))?;
            let new_mode = mode(fields.next().unwrap_or(""))?;
            let _ids = (fields.next(), fields.next());
            let status = status(fields.next().unwrap_or(""))?;
            let first = it.next().ok_or_else(|| unexpected("raw_without_path"))?;
            let (old_raw, path_raw) = if status == ChangeStatus::Renamed {
                let second = it.next().ok_or_else(|| unexpected("rename_without_path"))?;
                (Some(first), second)
            } else {
                (None, first)
            };
            let Ok(path) = std::str::from_utf8(path_raw) else {
                return Ok(RiskDiff::NonUtf8);
            };
            let old_path = match old_raw.map(std::str::from_utf8).transpose() {
                Ok(p) => p,
                Err(_) => return Ok(RiskDiff::NonUtf8),
            };
            if files.len() >= max {
                return Ok(RiskDiff::TooMany);
            }
            by_path.insert(path.to_owned(), files.len());
            files.push(ChangedFile {
                path: path.to_owned(),
                old_path: old_path.map(str::to_owned),
                status,
                old_mode,
                new_mode,
                added: None,
                deleted: None,
            });
        } else {
            let mut fields = text.splitn(3, '\t');
            let added = fields.next().ok_or_else(|| unexpected("numstat_shape"))?;
            let deleted = fields.next().ok_or_else(|| unexpected("numstat_shape"))?;
            let path = fields.next().ok_or_else(|| unexpected("numstat_shape"))?;
            let path = if path.is_empty() {
                // A rename: the old path, then the new one.
                let _old = it.next().ok_or_else(|| unexpected("numstat_rename"))?;
                let new = it.next().ok_or_else(|| unexpected("numstat_rename"))?;
                match std::str::from_utf8(new) {
                    Ok(p) => p.to_owned(),
                    Err(_) => return Ok(RiskDiff::NonUtf8),
                }
            } else {
                path.to_owned()
            };
            if let Some(file) = by_path.get(&path).and_then(|i| files.get_mut(*i)) {
                file.added = added.parse::<u64>().ok();
                file.deleted = deleted.parse::<u64>().ok();
            }
        }
    }
    Ok(RiskDiff::Complete(files))
}

/// The empty tree's id for the hash of `like` (git knows it without the
/// object being stored): the base of a "whole tree" `diff-tree`.
pub(crate) fn empty_tree(like: &ObjectId) -> ObjectId {
    let id = if like.as_str().len() == 64 {
        "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321"
    } else {
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
    };
    // A constant that is a valid object id; the fallback is `like` itself
    // (an empty diff), never a panic.
    ObjectId::parse(id).unwrap_or_else(|_| like.clone()) // clone: the fallback owns its id
}

/// Count the `--numstat -z` records of a whole-tree diff: files, and the
/// lines of the text ones. A record cut by the output cap is dropped.
pub(crate) fn parse_totals(stdout: &[u8], max_entries: usize, cut: bool) -> TreeTotals {
    let mut pieces: Vec<&[u8]> = stdout.split(|b| *b == 0).collect();
    pieces.pop(); // after the last NUL: empty when complete, a cut record otherwise
    let mut files: u64 = 0;
    let mut lines: u64 = 0;
    let mut truncated = cut;
    for piece in pieces {
        if usize::try_from(files).unwrap_or(usize::MAX) >= max_entries {
            truncated = true;
            break;
        }
        files += 1;
        let added = piece
            .split(|b| *b == b'\t')
            .next()
            .and_then(|a| std::str::from_utf8(a).ok())
            .and_then(|a| a.parse::<u64>().ok());
        lines = lines.saturating_add(added.unwrap_or(0));
    }
    TreeTotals {
        files,
        lines,
        truncated,
    }
}

impl Git {
    /// Every entry changed between `base` and `head` with its modes, line
    /// counts and renames paired (module docs), at most `max_entries`.
    pub async fn risk_diff(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        base: &ObjectId,
        head: &ObjectId,
        max_entries: usize,
        timeout: Duration,
    ) -> Result<RiskDiff, ForgeError> {
        let repo = storage.existing(id)?;
        // A raw record (≤ 200 bytes), a numstat record and the paths twice.
        let cap = max_entries.saturating_mul(3 * 4097 + 256).max(8192);
        let (output, truncated) = self
            .command("diff-tree")
            .arg("-M")
            .arg("-r")
            .arg("-z")
            .arg("--raw")
            .arg("--numstat")
            .revision(Revision::object_id(base.as_str())?)
            .revision(Revision::object_id(head.as_str())?)
            .repo(&repo)
            .timeout(timeout)
            .output_truncated(cap)
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        if truncated {
            return Ok(RiskDiff::TooMany);
        }
        parse_risk_diff(&output.stdout, max_entries)
    }

    /// The files and text lines of `commit`'s tree: a whole-tree
    /// `diff-tree --numstat` against the empty tree. Stops at
    /// `max_entries` files (or the output cap) with `truncated` set.
    pub async fn tree_totals(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        commit: &ObjectId,
        max_entries: usize,
        timeout: Duration,
    ) -> Result<TreeTotals, ForgeError> {
        let repo = storage.existing(id)?;
        let cap = max_entries.saturating_mul(4097 + 64).max(8192);
        let (output, truncated) = self
            .command("diff-tree")
            .arg("--no-renames")
            .arg("-r")
            .arg("-z")
            .arg("--numstat")
            .revision(Revision::object_id(empty_tree(commit).as_str())?)
            .revision(Revision::object_id(commit.as_str())?)
            .repo(&repo)
            .timeout(timeout)
            .output_truncated(cap)
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        Ok(parse_totals(&output.stdout, max_entries, truncated))
    }
}

#[cfg(test)]
#[path = "risk_inputs_tests.rs"]
mod tests;
