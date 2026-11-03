//! The shape of a change between two commits (M3 R6 review, ADR-0024
//! §8.2): what the approval carry needs besides the paths — each changed
//! entry's old and new mode (a file-mode change, a symlink, a submodule)
//! and its added / deleted line counts (`None` for a binary file).
//! [`Git::changed_entries`] runs `diff-tree --no-renames -r -z --raw
//! --numstat --end-of-options <base> <head>` once, bounded by the caller's
//! timeout and an output cap; nothing here decides what a change means.

use std::time::Duration;

use forge_core::{ObjectId, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use crate::command::Git;
use crate::revision::Revision;
use crate::storage::StorageRoot;

/// One changed entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedEntry {
    pub path: String,
    /// The octal mode before (`0`: added) and after (`0`: deleted):
    /// `100644` a file, `100755` an executable, `120000` a symlink,
    /// `160000` a submodule (gitlink).
    pub old_mode: u32,
    pub new_mode: u32,
    /// Lines added / deleted (`None`: binary, or a submodule).
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}

/// The changed entries between two commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeListing {
    Complete(Vec<ChangedEntry>),
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

/// Parse the `--raw --numstat -z` output: every raw record (`:<old>
/// <new> <old id> <new id> <status>` NUL `<path>` NUL) first, then every
/// numstat record (`<added>\t<deleted>\t<path>` NUL; `-` for binary).
pub(crate) fn parse(stdout: &[u8], max: usize) -> Result<ChangeListing, ForgeError> {
    let mut pieces: Vec<&[u8]> = stdout.split(|b| *b == 0).collect();
    pieces.pop(); // what follows the last NUL: empty for a complete output
    let mut entries: Vec<ChangedEntry> = Vec::new();
    let mut it = pieces.into_iter();
    let mut counts: Vec<(String, Option<u64>, Option<u64>)> = Vec::new();
    while let Some(record) = it.next() {
        let Ok(text) = std::str::from_utf8(record) else {
            return Ok(ChangeListing::NonUtf8);
        };
        if let Some(header) = text.strip_prefix(':') {
            let path = it.next().ok_or_else(|| unexpected("raw_without_path"))?;
            let Ok(path) = std::str::from_utf8(path) else {
                return Ok(ChangeListing::NonUtf8);
            };
            let mut fields = header.split(' ');
            let old_mode = mode(fields.next().unwrap_or(""))?;
            let new_mode = mode(fields.next().unwrap_or(""))?;
            if entries.len() >= max {
                return Ok(ChangeListing::TooMany);
            }
            entries.push(ChangedEntry {
                path: path.to_owned(),
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
            counts.push((
                path.to_owned(),
                added.parse::<u64>().ok(),
                deleted.parse::<u64>().ok(),
            ));
        }
    }
    for (path, added, deleted) in counts {
        if let Some(entry) = entries.iter_mut().find(|e| e.path == path) {
            entry.added = added;
            entry.deleted = deleted;
        }
    }
    Ok(ChangeListing::Complete(entries))
}

impl Git {
    /// Every entry changed between `base` and `head` with its modes and
    /// line counts (module docs), at most `max_entries`.
    pub async fn changed_entries(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        base: &ObjectId,
        head: &ObjectId,
        max_entries: usize,
        timeout: Duration,
    ) -> Result<ChangeListing, ForgeError> {
        let repo = storage.existing(id)?;
        // A raw record (≤ 200 bytes), a numstat record and the path twice.
        let cap = max_entries.saturating_mul(2 * 4097 + 256).max(8192);
        let (output, truncated) = self
            .command("diff-tree")
            .arg("--no-renames")
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
            return Ok(ChangeListing::TooMany);
        }
        parse(&output.stdout, max_entries)
    }
}

#[cfg(test)]
#[path = "review_changes_tests.rs"]
mod tests;
