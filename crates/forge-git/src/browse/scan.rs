//! The specification scans (M3 X4, see [`super`]): the regular files under
//! a commit's specification directories, several blobs read at once, every
//! text line naming `@req`, and a merge base — what forge-checks indexes
//! requirements and tags from. Every call is bounded like the rest of the
//! browse API (one deadline, the per-repository cap, an output cap that
//! truncates instead of buffering), and only resolved ids and validated
//! paths reach git:
//!
//! ```text
//! git ls-tree -r -z --long --full-tree --end-of-options <commit> -- <dir>…
//! git cat-file --batch                          (validated ids on stdin)
//! git grep -z -n -I --no-color -F -e <needle> --end-of-options <commit> --
//! git merge-base --end-of-options <a> <b>
//! ```
//!
//! `GIT_LITERAL_PATHSPECS=1` (forge-git's pinned environment) makes every
//! directory a literal prefix: no pathspec magic.

use std::time::Duration;

use forge_core::{ObjectId, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use super::path::RepoPath;
use super::tree::{log_skipped, parse_tree, EntryKind};
use super::{object_id, Browser, Deadline, TREE_OUTPUT_BYTES};
use crate::revision::Revision;

/// One regular file of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedBlob {
    /// The full path from the tree's root.
    pub path: String,
    pub oid: String,
    pub size: u64,
}

/// The regular files under some directories of a commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobListing {
    /// In git's order (byte order of the paths).
    pub blobs: Vec<ListedBlob>,
    /// More files than the cap (or the output was cut).
    pub truncated: bool,
}

/// One line found by [`Browser::grep_fixed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepHit {
    pub path: String,
    /// 1-based.
    pub line: usize,
    /// The line (lossy UTF-8).
    pub text: String,
}

/// The lines found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrepHits {
    /// In path order, then line order.
    pub hits: Vec<GrepHit>,
    /// git's output passed the cap: the lines after are not here.
    pub truncated: bool,
}

/// One `cat-file --batch` call reads at most this many bytes of content
/// (the caller reads more in several calls).
const BATCH_BYTES: u64 = 8 * 1024 * 1024;

/// Output of one merge-base (one id and a newline).
const MERGE_BASE_OUTPUT_BYTES: usize = 256;

fn unreadable(detail: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed)
        .with_context("reason", "scan_output_unreadable")
        .with_context("detail", detail)
}

impl Browser {
    /// The regular files (not symlinks, not submodules) under `dirs` of
    /// `commit`, at most `max` (more: `truncated`). No directory: nothing
    /// (never the whole tree). A directory that is not there lists nothing.
    pub async fn blobs_under(
        &self,
        repo: RepoId,
        commit: &str,
        dirs: &[RepoPath],
        max: usize,
    ) -> Result<BlobListing, ForgeError> {
        let dirs: Vec<&str> = dirs
            .iter()
            .filter(|d| !d.is_root())
            .map(|d| d.as_str().trim_end_matches('/'))
            .collect();
        if dirs.is_empty() {
            return Ok(BlobListing::default());
        }
        self.list_blobs(repo, commit, &dirs, max).await
    }

    /// Every regular file of `commit`'s tree, at most `max` (more, or more
    /// than the tree output cap: `truncated`). The document lint's one
    /// listing of a head (plan 13 stage 2): documents may be in any folder,
    /// and the specification directories are taken from the same listing.
    pub async fn blobs_in_tree(
        &self,
        repo: RepoId,
        commit: &str,
        max: usize,
    ) -> Result<BlobListing, ForgeError> {
        self.list_blobs(repo, commit, &[], max).await
    }

    /// `ls-tree -r` of `commit` (limited to `dirs` when there are any).
    async fn list_blobs(
        &self,
        repo: RepoId,
        commit: &str,
        dirs: &[&str],
        max: usize,
    ) -> Result<BlobListing, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let mut cmd = self
            .git
            .command("ls-tree")
            .arg("-r")
            .arg("-z")
            .arg("--long")
            .arg("--full-tree")
            .revision(Revision::object_id(commit)?);
        for d in dirs {
            cmd = cmd.path(d);
        }
        let (out, cut) = cmd
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(TREE_OUTPUT_BYTES)
            .await?;
        let parsed = parse_tree(&out.stdout, usize::MAX);
        log_skipped(repo, &parsed);
        let mut listing = BlobListing {
            blobs: Vec::new(),
            truncated: cut || parsed.truncated,
        };
        for e in parsed.entries {
            if e.kind != EntryKind::Blob {
                continue;
            }
            if listing.blobs.len() >= max {
                listing.truncated = true;
                break;
            }
            let size = e.blob_size()?;
            listing.blobs.push(ListedBlob {
                path: e.name,
                oid: e.oid,
                size,
            });
        }
        Ok(listing)
    }

    /// The first `max` bytes of one listed blob (`cat-file blob`, its
    /// output capped: a large file is never read whole). Plan 13 stage 2:
    /// a Markdown file over the document parser's limit is read only as far
    /// as the parser needs to say `too_large`.
    pub async fn read_blob_prefix(
        &self,
        repo: RepoId,
        blob: &ListedBlob,
        max: usize,
    ) -> Result<Vec<u8>, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let rev = Revision::object_id(&blob.oid)?;
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let (out, _cut) = self
            .git
            .command("cat-file")
            .arg("blob")
            .revision(rev)
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(max)
            .await?;
        let mut bytes = out.stdout;
        bytes.truncate(max);
        Ok(bytes)
    }

    /// The contents of `blobs` (as listed), in order: `cat-file --batch`,
    /// as many per call as stay within [`BATCH_BYTES`] by their listed
    /// sizes. Only full object ids reach git (anything else is
    /// `FG-GIT-0005 reason=bad_object_id`); the caller bounds the total by
    /// the sizes it listed. An id git cannot find is `FG-GIT-0003`. One
    /// deadline for every call.
    pub async fn read_blobs(
        &self,
        repo: RepoId,
        blobs: &[ListedBlob],
    ) -> Result<Vec<Vec<u8>>, ForgeError> {
        let mut ids = Vec::with_capacity(blobs.len());
        for b in blobs {
            ids.push((Revision::object_id(&b.oid)?, b.size));
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let deadline = Deadline::new(self.limits.timeout);
        let dir = self.storage.existing(repo)?;
        let mut out = Vec::with_capacity(ids.len());
        let mut rest = ids.as_slice();
        while !rest.is_empty() {
            let take = batch_len(rest);
            let (chunk, after) = rest.split_at(take);
            rest = after;
            let _slot = self.claim(repo)?;
            let stdin: String = chunk
                .iter()
                .map(|(r, _)| format!("{}\n", r.as_str()))
                .collect();
            // The listed sizes plus each header: more is not what was listed.
            let expected: u64 = chunk.iter().map(|(_, size)| size.saturating_add(128)).sum();
            let cap = usize::try_from(expected).unwrap_or(usize::MAX);
            let run = self
                .git
                .command("cat-file")
                .arg("--batch")
                .repo(&dir)
                .stdin(stdin.into_bytes())
                .timeout(deadline.left()?);
            let (captured, cut) = run.output_truncated(cap).await?;
            if cut {
                return Err(unreadable("batch_too_large").with_context("repo_id", repo.get()));
            }
            let blobs = split_batch(&captured.stdout)?;
            if blobs.len() != chunk.len() {
                return Err(unreadable("batch_count_mismatch").with_context("repo_id", repo.get()));
            }
            out.extend(blobs.into_iter().map(<[u8]>::to_vec));
        }
        Ok(out)
    }

    /// Every line of a text file of `commit` that contains `needle`
    /// literally (`git grep -F`; binary files are skipped). At most
    /// `max_bytes` of git's output are read (more: `truncated`, the lines
    /// read whole are kept); `timeout` bounds the run (`FG-GIT-0002`). git
    /// exits 1 when nothing matches: an empty answer.
    pub async fn grep_fixed(
        &self,
        repo: RepoId,
        commit: &str,
        needle: &'static str,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<GrepHits, ForgeError> {
        let rev = Revision::object_id(commit)?;
        let prefix = format!("{}:", rev.as_str());
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let captured = self
            .git
            .command("grep")
            .arg("-z")
            .arg("-n")
            .arg("-I")
            .arg("--no-color")
            .arg("--no-textconv")
            .arg("-F")
            .arg("-e")
            .arg(needle)
            // No pathspec: the whole tree. The operand is a validated full
            // id, without `--end-of-options`: `git grep` before 2.44 refuses
            // that marker (found on git 2.43; the minimum is 2.38).
            .object_id_operand(&rev)?
            .repo(&dir)
            .timeout(timeout)
            .output_accepting(max_bytes, &[0, 1])
            .await
            .map_err(|e| e.with_context("repo_id", repo.get()))?;
        let truncated = captured.truncated;
        if !truncated && !matches!(captured.exit_code, Some(0 | 1)) {
            return Err(unreadable("grep_exit").with_context("repo_id", repo.get()));
        }
        let mut hits = GrepHits {
            hits: Vec::new(),
            truncated,
        };
        let out = &captured.output.stdout;
        // `<commit>:<path>\0<line>\0<text>\n` per line; a record cut by the
        // cap has no final newline and is dropped.
        let complete = out.ends_with(b"\n");
        let mut records: Vec<&[u8]> = out.split(|b| *b == b'\n').collect();
        records.pop();
        if !complete && !out.is_empty() {
            hits.truncated = true;
        }
        for record in records {
            let mut fields = record.splitn(3, |b| *b == 0);
            let (Some(name), Some(number), Some(text)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(unreadable("grep_record").with_context("repo_id", repo.get()));
            };
            let name = String::from_utf8_lossy(name);
            let Some(path) = name.strip_prefix(prefix.as_str()) else {
                return Err(unreadable("grep_prefix").with_context("repo_id", repo.get()));
            };
            let line = std::str::from_utf8(number)
                .ok()
                .and_then(|n| n.parse::<usize>().ok())
                .ok_or_else(|| unreadable("grep_line").with_context("repo_id", repo.get()))?;
            let text = String::from_utf8_lossy(text);
            hits.hits.push(GrepHit {
                path: path.to_owned(),
                line,
                text: text.strip_suffix('\r').unwrap_or(&text).to_owned(),
            });
        }
        Ok(hits)
    }

    /// The best common ancestor of `a` and `b` in `repo` (`None`: no shared
    /// history), bounded by the browse timeout.
    pub async fn merge_base(
        &self,
        repo: RepoId,
        a: &ObjectId,
        b: &ObjectId,
    ) -> Result<Option<ObjectId>, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let captured = self
            .git
            .command("merge-base")
            .revision(Revision::object_id(a.as_str())?)
            .revision(Revision::object_id(b.as_str())?)
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_accepting(MERGE_BASE_OUTPUT_BYTES, &[0, 1])
            .await
            .map_err(|e| e.with_context("repo_id", repo.get()))?;
        match captured.exit_code {
            Some(0) => {
                let text = captured.output.stdout_str();
                let id = object_id(text.trim()).ok_or_else(|| unreadable("merge_base"))?;
                ObjectId::parse(&id)
                    .map(Some)
                    .map_err(|_| unreadable("merge_base"))
            }
            Some(1) => Ok(None),
            Some(_) | None => Err(unreadable("merge_base_exit")),
        }
    }
}

/// How many of `rest` one `cat-file --batch` call reads: at least one,
/// then while the listed sizes stay within [`BATCH_BYTES`].
fn batch_len(rest: &[(Revision, u64)]) -> usize {
    let mut bytes = 0u64;
    let mut n = 0;
    for (_, size) in rest {
        if n > 0 && bytes.saturating_add(*size) > BATCH_BYTES {
            break;
        }
        bytes = bytes.saturating_add(*size);
        n += 1;
    }
    n.clamp(1, rest.len())
}

/// `cat-file --batch` output: `<id> <type> <size>\n<content>\n` per
/// object;`<id> missing\n` for one git cannot find (`FG-GIT-0003`).
fn split_batch(out: &[u8]) -> Result<Vec<&[u8]>, ForgeError> {
    let mut blobs = Vec::new();
    let mut rest = out;
    while !rest.is_empty() {
        let nl = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| unreadable("batch_header"))?;
        let header = std::str::from_utf8(rest.get(..nl).unwrap_or_default())
            .map_err(|_| unreadable("batch_header"))?;
        if header.ends_with(" missing") {
            return Err(unreadable("batch_object_missing"));
        }
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| unreadable("batch_header"))?;
        let start = nl + 1;
        // Review X4 (LOW): a size git never prints cannot overflow here.
        let end = start
            .checked_add(size)
            .ok_or_else(|| unreadable("batch_content"))?;
        let content = rest
            .get(start..end)
            .ok_or_else(|| unreadable("batch_content"))?;
        blobs.push(content);
        rest = rest.get(end.saturating_add(1)..).unwrap_or_default();
    }
    Ok(blobs)
}
