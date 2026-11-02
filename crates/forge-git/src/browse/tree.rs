//! Trees (see [`super`]): `ls-tree` listings, one entry at a path, and
//! parsing `ls-tree -z --long`.

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};

use super::path::RepoPath;
use super::{not_resolved, object_id, Browser, Deadline, TREE_OUTPUT_BYTES};
use crate::revision::Revision;

/// What a tree entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Tree,
    Blob,
    /// A blob with mode `120000`.
    Symlink,
    /// A gitlink (mode `160000`, a commit of another repository).
    Submodule,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Blob => "blob",
            Self::Symlink => "symlink",
            Self::Submodule => "submodule",
        }
    }

    pub(super) fn is_blob(self) -> bool {
        matches!(self, Self::Blob | Self::Symlink)
    }
}

/// One entry of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// The entry's name (lossy UTF-8 for a name that is not).
    pub name: String,
    pub kind: EntryKind,
    /// The octal mode as git prints it (`100644`, `040000`, …).
    pub mode: String,
    pub oid: String,
    /// Blobs only.
    pub size: Option<u64>,
}

impl TreeEntry {
    /// A blob's size; `FG-GIT-0003 reason=blob_size_missing` when git
    /// printed none for a blob (never for a real blob: `ls-tree --long`
    /// always prints it).
    pub fn blob_size(&self) -> Result<u64, ForgeError> {
        self.size.ok_or_else(|| {
            ForgeError::new(ErrorCode::GitProcessFailed)
                .with_context("reason", "blob_size_missing")
                .with_context("oid", &self.oid)
        })
    }
}

/// A tree listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    /// The tree's own id.
    pub oid: String,
    /// In git's order (byte order of the names).
    pub entries: Vec<TreeEntry>,
    /// More than [`BrowseLimits::max_tree_entries`] entries.
    pub truncated: bool,
}

impl Browser {
    /// The entry at `path` in `commit` (`None`: no such entry).
    pub(super) async fn entry_at(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
        deadline: Deadline,
    ) -> Result<Option<TreeEntry>, ForgeError> {
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let (out, cut) = self
            .git
            .command("ls-tree")
            .arg("-z")
            .arg("--long")
            .arg("--full-tree")
            .revision(Revision::object_id(commit)?)
            .path(path.as_str())
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(64 * 1024)
            .await?;
        if cut {
            // One literal path names one entry: more output than that is
            // not git's answer to this question.
            return Err(ForgeError::new(ErrorCode::GitProcessFailed)
                .with_context("reason", "ls_tree_truncated")
                .with_context("repo_id", repo.get()));
        }
        let parsed = parse_tree(&out.stdout, usize::MAX);
        log_skipped(repo, &parsed);
        Ok(parsed
            .entries
            .into_iter()
            .find(|e| e.name == path.as_str())
            .map(|mut e| {
                e.name = path.file_name().unwrap_or_default().to_owned();
                e
            }))
    }

    /// The tree at `path` of `commit`.
    pub async fn tree(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
    ) -> Result<Tree, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let target = if path.is_root() {
            commit.to_owned()
        } else {
            match self.entry_at(repo, commit, path, deadline).await? {
                None => return Err(not_resolved("path_not_found")),
                Some(e) if e.kind == EntryKind::Tree => e.oid,
                Some(_) => return Err(not_resolved("not_a_tree")),
            }
        };
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let (out, truncated_bytes) = self
            .git
            .command("ls-tree")
            .arg("-z")
            .arg("--long")
            .revision(Revision::object_id(&target)?)
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(TREE_OUTPUT_BYTES)
            .await?;
        let parsed = parse_tree(&out.stdout, self.limits.max_tree_entries);
        log_skipped(repo, &parsed);
        let ParsedTree {
            entries, truncated, ..
        } = parsed;
        let oid = if path.is_root() {
            self.tree_of(repo, commit, deadline).await?
        } else {
            target
        };
        Ok(Tree {
            oid,
            entries,
            truncated: truncated || truncated_bytes,
        })
    }

    /// The root tree id of `commit`.
    async fn tree_of(
        &self,
        repo: RepoId,
        commit: &str,
        deadline: Deadline,
    ) -> Result<String, ForgeError> {
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let out = self
            .git
            .command("cat-file")
            .arg("--batch-check=%(objectname)")
            .repo(&dir)
            .stdin(format!("{commit}^{{tree}}\n").into_bytes())
            .timeout(deadline.left()?)
            .output()
            .await?;
        let text = out.stdout_str();
        let oid = text.trim();
        if object_id(oid).is_none() {
            return Err(ForgeError::new(ErrorCode::GitProcessFailed)
                .with_context("reason", "tree_of_commit_unreadable")
                .with_context("repo_id", repo.get()));
        }
        Ok(oid.to_owned())
    }
}

/// What [`parse_tree`] read.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ParsedTree {
    pub(super) entries: Vec<TreeEntry>,
    /// More entries followed (`max`), or the output was cut.
    pub(super) truncated: bool,
    /// Records that could not be read (no tab, too few fields, an unknown
    /// type) — git's output is not what forge expects.
    pub(super) malformed: usize,
    /// Entries whose name is not UTF-8: left out (they could not be asked
    /// for again through the API).
    pub(super) not_utf8: usize,
}

/// Records [`parse_tree`] left out, said once per call (warn, with the
/// repository): never silently dropped.
fn log_skipped(repo: RepoId, parsed: &ParsedTree) {
    if parsed.malformed > 0 || parsed.not_utf8 > 0 {
        tracing::warn!(
            target: "forge_git::browse",
            repo_id = repo.get(),
            error_code = ErrorCode::GitProcessFailed.id(),
            malformed = parsed.malformed,
            not_utf8 = parsed.not_utf8,
            "forge.git.browse.tree_entries_skipped"
        );
    }
}

/// `ls-tree -z --long` records → entries (at most `max`), whether more
/// followed, and how many records were left out. A record cut by the
/// output cap is dropped (and marks the listing truncated).
pub(super) fn parse_tree(out: &[u8], max: usize) -> ParsedTree {
    let mut parsed = ParsedTree::default();
    let complete = out.ends_with(b"\0") || out.is_empty();
    let mut records: Vec<&[u8]> = out.split(|b| *b == 0).collect();
    // The split leaves an empty tail after the final NUL, or a partial
    // record when the output was cut.
    records.pop();
    if !complete {
        parsed.truncated = true;
    }
    for record in records {
        let Some(tab) = record.iter().position(|b| *b == b'\t') else {
            parsed.malformed += 1;
            continue;
        };
        let (head, name) = (
            record.get(..tab).unwrap_or_default(),
            record.get(tab + 1..).unwrap_or_default(),
        );
        let head = String::from_utf8_lossy(head);
        let mut fields = head.split_whitespace();
        let (Some(mode), Some(kind), Some(oid), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            parsed.malformed += 1;
            continue;
        };
        let kind = match (kind, mode) {
            ("tree", _) => EntryKind::Tree,
            ("blob", "120000") => EntryKind::Symlink,
            ("blob", _) => EntryKind::Blob,
            ("commit", _) => EntryKind::Submodule,
            _ => {
                parsed.malformed += 1;
                continue;
            }
        };
        let Ok(name) = std::str::from_utf8(name) else {
            parsed.not_utf8 += 1;
            continue;
        };
        if parsed.entries.len() >= max {
            parsed.truncated = true;
            break;
        }
        parsed.entries.push(TreeEntry {
            name: name.to_owned(),
            kind,
            mode: mode.to_owned(),
            oid: oid.to_owned(),
            size: size.parse().ok(),
        });
    }
    parsed
}
