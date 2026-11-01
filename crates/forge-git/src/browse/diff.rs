//! Diffs (M2 MR8, plan 10 §5 `diff-tree --patch`): the changed files of
//! two trees (or of a root commit against nothing), with per-file patches
//! under caps.
//!
//! Two `diff-tree` runs share one command shape: `--raw -z --no-abbrev`
//! gives the exact file list (NUL-separated, so no path is quoted or
//! guessed), `-p` gives the patch text, which is split at its
//! `diff --git ` header lines (a hunk line always starts with ` `, `+`,
//! `-`, `\` or `@`, so such a line is a header) and matched to the files in
//! order — a type change is two chunks (git writes a delete and a create).
//! A patch that does not fit the list is `FG-GIT-0003
//! reason=diff_chunk_mismatch`, never a guess.
//!
//! Caps: files ([`super::BrowseLimits::max_diff_files`], `truncated`),
//! patch bytes ([`super::BrowseLimits::max_diff_bytes`], git is killed
//! there; the files after are [`PatchOmitted::TotalCap`]), one file's patch
//! ([`super::BrowseLimits::max_file_patch_bytes`], [`PatchOmitted::TooLarge`]).
//! A patch is returned whole or not at all. External diff drivers and
//! textconv are switched off.

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};

use super::{Browser, Deadline};
use crate::revision::Revision;

/// Bytes of `--raw` output read at most (the file cap decides first).
const RAW_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// How a file changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

impl DiffStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::TypeChanged => "type_changed",
        }
    }
}

/// Why a file has no patch although it has hunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOmitted {
    /// The patch is longer than [`super::BrowseLimits::max_file_patch_bytes`].
    TooLarge,
    /// The diff's patch bytes ran out before this file.
    TotalCap,
    /// The patch is not UTF-8 text.
    NotUtf8,
}

impl PatchOmitted {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::TotalCap => "total_cap",
            Self::NotUtf8 => "not_utf8",
        }
    }

    /// Cut by a cap (not a property of the file's content).
    pub fn is_cap(self) -> bool {
        matches!(self, Self::TooLarge | Self::TotalCap)
    }
}

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub status: DiffStatus,
    /// The path before (`None` for an added file).
    pub old_path: Option<String>,
    /// The path after (`None` for a deleted file).
    pub new_path: Option<String>,
    pub old_mode: String,
    pub new_mode: String,
    pub old_oid: String,
    pub new_oid: String,
    /// Renamed / copied: the similarity in percent.
    pub similarity: Option<u8>,
    /// git reports the change as binary: no patch.
    pub binary: bool,
    /// The hunks (`@@ …`), `""` for a change without any (mode only, a
    /// pure rename, an empty file); `None`: see `omitted` (or `binary`).
    pub patch: Option<String>,
    pub omitted: Option<PatchOmitted>,
}

/// The changed files of a diff.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diff {
    pub files: Vec<DiffFile>,
    /// A cap cut the diff: more files than listed, or a patch left out by
    /// a size cap.
    pub truncated: bool,
}

/// What [`parse_raw`] read.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ParsedRaw {
    pub(super) files: Vec<DiffFile>,
    /// More entries followed (`max`), or the output was cut.
    pub(super) truncated: bool,
}

fn malformed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed).with_context("reason", reason)
}

/// `diff-tree --raw -z --no-abbrev` output → files (at most `max`). `cut`:
/// the output was truncated (an incomplete last entry is dropped and the
/// diff is `truncated`). A malformed entry in complete output is an error.
pub(super) fn parse_raw(out: &[u8], cut: bool, max: usize) -> Result<ParsedRaw, ForgeError> {
    let mut parsed = ParsedRaw::default();
    let mut tokens: Vec<&[u8]> = out.split(|b| *b == 0).collect();
    // The split leaves an empty tail after the final NUL (or a partial
    // token when the output was cut).
    let tail = tokens.pop();
    if cut {
        parsed.truncated = true;
    } else if tail.is_some_and(|t| !t.is_empty()) {
        return Err(malformed("diff_raw_unterminated"));
    }
    let mut it = tokens.into_iter().peekable();
    while let Some(meta) = it.next() {
        let Some(meta) = meta.strip_prefix(b":") else {
            return Err(malformed("diff_raw_meta"));
        };
        let meta = std::str::from_utf8(meta).map_err(|_| malformed("diff_raw_meta"))?;
        let fields: Vec<&str> = meta.split(' ').collect();
        let [old_mode, new_mode, old_oid, new_oid, status] = fields.as_slice() else {
            return Err(malformed("diff_raw_meta"));
        };
        let letter = status.chars().next().unwrap_or(' ');
        let (kind, two_paths) = match letter {
            'A' => (DiffStatus::Added, false),
            'M' => (DiffStatus::Modified, false),
            'D' => (DiffStatus::Deleted, false),
            'T' => (DiffStatus::TypeChanged, false),
            'R' => (DiffStatus::Renamed, true),
            'C' => (DiffStatus::Copied, true),
            _ => return Err(malformed("diff_raw_status")),
        };
        let similarity = if two_paths {
            status.get(1..).and_then(|s| s.parse::<u8>().ok())
        } else {
            None
        };
        let wanted = if two_paths { 2 } else { 1 };
        let mut paths: Vec<String> = Vec::with_capacity(wanted);
        for _ in 0..wanted {
            match it.next() {
                Some(p) => paths.push(
                    String::from_utf8(p.to_vec()).map_err(|_| malformed("diff_path_not_utf8"))?,
                ),
                None if cut => return Ok(parsed),
                None => return Err(malformed("diff_raw_paths")),
            }
        }
        if parsed.files.len() >= max {
            parsed.truncated = true;
            break;
        }
        let (old_path, new_path) = match (kind, paths.as_slice()) {
            (DiffStatus::Added, [p]) => (None, Some(p.clone())), // clone: both sides own a copy
            (DiffStatus::Deleted, [p]) => (Some(p.clone()), None), // clone: as above
            (_, [p]) => (Some(p.clone()), Some(p.clone())),      // clone: as above
            (_, [a, b]) => (Some(a.clone()), Some(b.clone())),   // clone: as above
            _ => return Err(malformed("diff_raw_paths")),
        };
        parsed.files.push(DiffFile {
            status: kind,
            old_path,
            new_path,
            old_mode: (*old_mode).to_owned(),
            new_mode: (*new_mode).to_owned(),
            old_oid: (*old_oid).to_owned(),
            new_oid: (*new_oid).to_owned(),
            similarity,
            binary: false,
            patch: None,
            omitted: None,
        });
    }
    Ok(parsed)
}

/// One file's part of a patch: whether git calls it binary, and its hunks.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Chunk<'a> {
    pub(super) binary: bool,
    /// From the first `@@` line on (empty: no hunks).
    pub(super) hunks: &'a [u8],
}

/// Split a patch at its `diff --git ` header lines.
pub(super) fn split_chunks(patch: &[u8]) -> Vec<Chunk<'_>> {
    let mut starts: Vec<usize> = Vec::new();
    let mut pos = 0;
    while pos < patch.len() {
        let rest = patch.get(pos..).unwrap_or_default();
        if rest.starts_with(b"diff --git ") {
            starts.push(pos);
        }
        match rest.iter().position(|b| *b == b'\n') {
            Some(n) => pos += n + 1,
            None => break,
        }
    }
    let mut chunks = Vec::with_capacity(starts.len());
    for (i, start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(patch.len());
        let body = patch.get(*start..end).unwrap_or_default();
        chunks.push(chunk_of(body));
    }
    chunks
}

fn chunk_of(body: &[u8]) -> Chunk<'_> {
    let mut binary = false;
    let mut pos = 0;
    while pos < body.len() {
        let rest = body.get(pos..).unwrap_or_default();
        if rest.starts_with(b"@@ ") {
            return Chunk {
                binary,
                hunks: rest,
            };
        }
        if rest.starts_with(b"Binary files ") || rest.starts_with(b"GIT binary patch") {
            binary = true;
        }
        match rest.iter().position(|b| *b == b'\n') {
            Some(n) => pos += n + 1,
            None => break,
        }
    }
    Chunk { binary, hunks: &[] }
}

/// Attach the patch chunks to the files (in order). `cut`: the patch
/// output was truncated (its last chunk may be partial and is dropped; the
/// files after are left out by the total cap). Returns whether a size cap
/// cut anything.
pub(super) fn attach_patches(
    files: &mut [DiffFile],
    patch: &[u8],
    cut: bool,
    max_file: usize,
) -> Result<bool, ForgeError> {
    let mut chunks = split_chunks(patch);
    if cut {
        chunks.pop();
    }
    let mut next = chunks.into_iter();
    let mut capped = false;
    let mut ran_out = false;
    for file in files.iter_mut() {
        let need = if file.status == DiffStatus::TypeChanged {
            2
        } else {
            1
        };
        let mut mine: Vec<Chunk<'_>> = Vec::with_capacity(need);
        for _ in 0..need {
            match next.next() {
                Some(c) => mine.push(c),
                None => break,
            }
        }
        if mine.len() < need {
            if !cut {
                return Err(malformed("diff_chunk_mismatch"));
            }
            ran_out = true;
        }
        if ran_out {
            file.omitted = Some(PatchOmitted::TotalCap);
            capped = true;
            continue;
        }
        file.binary = mine.iter().any(|c| c.binary);
        if file.binary {
            continue;
        }
        let total: usize = mine.iter().map(|c| c.hunks.len()).sum();
        if total > max_file {
            file.omitted = Some(PatchOmitted::TooLarge);
            capped = true;
            continue;
        }
        let mut bytes: Vec<u8> = Vec::with_capacity(total);
        for c in &mine {
            bytes.extend_from_slice(c.hunks);
        }
        match String::from_utf8(bytes) {
            Ok(text) => file.patch = Some(text),
            Err(_) => file.omitted = Some(PatchOmitted::NotUtf8),
        }
    }
    if !cut && next.next().is_some() {
        return Err(malformed("diff_chunk_mismatch"));
    }
    Ok(capped)
}

impl Browser {
    /// The files changed from `base` to `head` (commits; `None`: `head` is
    /// a root commit, compared with nothing).
    pub(super) async fn diff(
        &self,
        repo: RepoId,
        base: Option<&str>,
        head: &str,
        deadline: Deadline,
    ) -> Result<Diff, ForgeError> {
        let raw = self
            .diff_tree(repo, base, head, false, RAW_OUTPUT_BYTES, deadline)
            .await?;
        let mut parsed = parse_raw(&raw.0, raw.1, self.limits.max_diff_files)?;
        let mut truncated = parsed.truncated;
        if !parsed.files.is_empty() {
            let (patch, cut) = self
                .diff_tree(
                    repo,
                    base,
                    head,
                    true,
                    self.limits.max_diff_bytes.saturating_add(1),
                    deadline,
                )
                .await?;
            // The file cap also bounds which chunks matter: a patch of more
            // files than listed is cut there, not a mismatch.
            let more_files = parsed.truncated;
            let capped = attach_patches(
                &mut parsed.files,
                &patch,
                cut || more_files,
                self.limits.max_file_patch_bytes,
            )?;
            truncated |= capped;
            // Past the total cap, "cut" came from git being stopped: the
            // files without a patch say so (`total_cap`) and the diff is
            // truncated, which `capped` already records.
        }
        Ok(Diff {
            files: parsed.files,
            truncated,
        })
    }

    /// One `diff-tree` run: `--raw -z` (the file list) or `-p` (patches).
    async fn diff_tree(
        &self,
        repo: RepoId,
        base: Option<&str>,
        head: &str,
        patch: bool,
        cap: usize,
        deadline: Deadline,
    ) -> Result<(Vec<u8>, bool), ForgeError> {
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let mut cmd = self
            .git
            .command("diff-tree")
            .arg("-r")
            .arg("-M")
            .arg("--no-ext-diff")
            .arg("--no-textconv")
            .arg("--no-color");
        cmd = if patch {
            cmd.arg("-p").arg("-U3")
        } else {
            cmd.arg("--raw").arg("-z").arg("--no-abbrev")
        };
        cmd = match base {
            Some(base) => cmd
                .revision(Revision::object_id(base)?)
                .revision(Revision::object_id(head)?),
            None => cmd
                .arg("--root")
                .arg("--no-commit-id")
                .revision(Revision::object_id(head)?),
        };
        let (out, cut) = cmd
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(cap)
            .await?;
        Ok((out.stdout, cut))
    }
}
