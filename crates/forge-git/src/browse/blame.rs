//! Blame (M2 MR8, plan 10 §5 `blame --porcelain`): who last changed each
//! line of a file at a commit.
//!
//! `git blame --porcelain -L 1,<cap+1> <commit> -- <path>` (the commit is a
//! resolved full hex id; git's blame does not take `--end-of-options`; the
//! path comes after `--`). The time limit
//! is [`super::BrowseLimits::blame_timeout`] (20 s); a file longer than
//! [`super::BrowseLimits::max_blame_lines`] is blamed for its first lines
//! only (`truncated`). A file with a NUL byte or a line that is not UTF-8
//! is `binary`: no lines (the raw route serves it).

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};

use super::path::RepoPath;
use super::tree::TreeEntry;
use super::{object_id, Browser, Deadline};
use crate::revision::Revision;

/// Bytes of porcelain output read at most (a line costs its text plus a
/// header; the line cap decides first on ordinary files).
const BLAME_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// A commit that last changed some lines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlameCommit {
    pub oid: String,
    pub author_name: String,
    pub author_email: String,
    /// Seconds since the epoch.
    pub author_time: i64,
    /// `+0900`.
    pub author_tz: String,
    pub summary: String,
    /// The root of the history git blamed (the line is as old as it goes).
    pub boundary: bool,
    /// The file's name in that commit (it may have been renamed since).
    pub path: String,
}

/// Consecutive lines last changed by one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameHunk {
    /// Index into [`Blame::commits`].
    pub commit: usize,
    /// The first line's number in the blamed file (1-based).
    pub start: usize,
    /// The first line's number in `commit`'s version.
    pub orig_start: usize,
    pub lines: Vec<String>,
}

/// A blamed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blame {
    pub entry: TreeEntry,
    pub size: u64,
    /// In file order.
    pub hunks: Vec<BlameHunk>,
    /// In order of first appearance.
    pub commits: Vec<BlameCommit>,
    /// More lines than [`super::BrowseLimits::max_blame_lines`].
    pub truncated: bool,
    /// NUL bytes or not UTF-8: no hunks.
    pub binary: bool,
}

/// What [`parse_blame`] read.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ParsedBlame {
    pub(super) hunks: Vec<BlameHunk>,
    pub(super) commits: Vec<BlameCommit>,
    pub(super) truncated: bool,
    pub(super) binary: bool,
    /// Metadata lines (names, summaries, file names) that were not UTF-8.
    pub(super) lossy: usize,
}

fn malformed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed).with_context("reason", reason)
}

/// `<oid> <orig line> <final line> [<group size>]`.
fn header(line: &str) -> Option<(String, usize, usize)> {
    let mut fields = line.split(' ');
    let oid = object_id(fields.next()?)?;
    let orig = fields.next()?.parse().ok()?;
    let fin = fields.next()?.parse().ok()?;
    match fields.next() {
        None => {}
        Some(n) => {
            n.parse::<usize>().ok()?;
            if fields.next().is_some() {
                return None;
            }
        }
    }
    Some((oid, orig, fin))
}

/// `blame --porcelain` output → hunks and the commits they name, at most
/// `max_lines` lines (more: `truncated`). `cut`: the output was truncated
/// (a line group whose text did not arrive is dropped, and the file is
/// `truncated`).
pub(super) fn parse_blame(
    out: &[u8],
    cut: bool,
    max_lines: usize,
) -> Result<ParsedBlame, ForgeError> {
    let mut parsed = ParsedBlame::default();
    let mut lines: Vec<&[u8]> = out.split(|b| *b == b'\n').collect();
    // The final element is what follows the last newline: empty, or a
    // partial line.
    lines.pop();
    if cut {
        parsed.truncated = true;
    }
    let mut current: Option<(usize, usize, usize)> = None; // (commit index, orig line, final line)
    let mut seen = 0usize;
    for raw in lines {
        if let Some(text) = raw.strip_prefix(b"\t") {
            let Some((commit, orig, fin)) = current.take() else {
                return Err(malformed("blame_line_without_header"));
            };
            if text.contains(&0) {
                parsed.binary = true;
                continue;
            }
            let Ok(text) = std::str::from_utf8(text) else {
                parsed.binary = true;
                continue;
            };
            seen += 1;
            if seen > max_lines {
                parsed.truncated = true;
                break;
            }
            let extends = parsed.hunks.last_mut().filter(|h| {
                h.commit == commit
                    && h.start + h.lines.len() == fin
                    && h.orig_start + h.lines.len() == orig
            });
            match extends {
                Some(h) => h.lines.push(text.to_owned()),
                None => parsed.hunks.push(BlameHunk {
                    commit,
                    start: fin,
                    orig_start: orig,
                    lines: vec![text.to_owned()],
                }),
            }
            continue;
        }
        // Not a content line: a header or a commit's metadata (text git
        // wrote from the commit; names may not be UTF-8 — lossy).
        let line = String::from_utf8_lossy(raw);
        if matches!(line, std::borrow::Cow::Owned(_)) {
            parsed.lossy += 1;
        }
        if let Some((oid, orig, fin)) = header(&line) {
            let index = match parsed.commits.iter().position(|c| c.oid == oid) {
                Some(i) => i,
                None => {
                    parsed.commits.push(BlameCommit {
                        oid,
                        ..BlameCommit::default()
                    });
                    parsed.commits.len() - 1
                }
            };
            current = Some((index, orig, fin));
            continue;
        }
        // Metadata of the commit named by the latest header.
        let Some((index, _, _)) = current else {
            return Err(malformed("blame_metadata_without_header"));
        };
        let Some(commit) = parsed.commits.get_mut(index) else {
            return Err(malformed("blame_metadata_without_header"));
        };
        let (key, value) = line.split_once(' ').unwrap_or((&line, ""));
        match key {
            "author" => commit.author_name = value.to_owned(),
            "author-mail" => {
                commit.author_email = value
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned();
            }
            "author-time" => {
                commit.author_time = value.parse().map_err(|_| malformed("blame_author_time"))?;
            }
            "author-tz" => commit.author_tz = value.to_owned(),
            "summary" => commit.summary = value.to_owned(),
            "boundary" => commit.boundary = true,
            "filename" => commit.path = value.to_owned(),
            // committer*, previous, … : not part of the answer.
            _ => {}
        }
    }
    if parsed.binary {
        parsed.hunks.clear();
        parsed.commits.clear();
        parsed.truncated = false;
    }
    Ok(parsed)
}

impl Browser {
    /// Blame the file at `path` of `commit` (the blob entry is checked
    /// first: `not_a_blob` / `path_not_found`).
    pub async fn blame(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
    ) -> Result<Blame, ForgeError> {
        let deadline = Deadline::new(self.limits.blame_timeout);
        let entry = self.blob_entry(repo, commit, path, deadline).await?;
        let size = entry.blob_size()?;
        if size == 0 {
            // `-L` on an empty file is a git error; there is nothing to say.
            return Ok(Blame {
                entry,
                size,
                hunks: Vec::new(),
                commits: Vec::new(),
                truncated: false,
                binary: false,
            });
        }
        let max_lines = self.limits.max_blame_lines.max(1);
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let (out, cut) = self
            .git
            .command("blame")
            .arg("--porcelain")
            .arg("-L")
            .arg_owned(format!("1,{}", max_lines + 1))
            // `blame` rejects `--end-of-options` (it parses its own
            // options), so the revision is the one operand before `--`: a
            // validated full hex id, never option-like, and `--` follows.
            .arg_owned(Revision::object_id(commit)?.as_str())
            .path(path.as_str())
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(BLAME_OUTPUT_BYTES)
            .await?;
        let parsed = parse_blame(&out.stdout, cut, max_lines)?;
        if parsed.lossy > 0 {
            tracing::warn!(
                target: "forge_git::browse",
                repo_id = repo.get(),
                error_code = ErrorCode::GitProcessFailed.id(),
                count = parsed.lossy,
                "forge.git.browse.log_lossy"
            );
        }
        Ok(Blame {
            entry,
            size,
            hunks: parsed.hunks,
            commits: parsed.commits,
            truncated: parsed.truncated,
            binary: parsed.binary,
        })
    }
}
