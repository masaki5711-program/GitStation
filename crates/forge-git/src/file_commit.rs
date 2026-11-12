//! One file committed without a working tree (the simple editor E1,
//! 2026-11-12): the content becomes a blob, the parent's tree with that one
//! path replaced becomes a tree, and the tree a commit — all with git
//! plumbing (argv, timeouts, hooks pinned to the empty directory), never a
//! checkout and never a tree computed by forge.
//!
//! - [`FilePath::parse`]: the path rules (`FG-GIT-0028`, `rule`): relative,
//!   `/`-separated, no empty / `.` / `..` segment, no `\`, no control
//!   character, at most 4096 bytes, and no segment that a developer's file
//!   system reads as `.git` (NTFS / HFS+: trailing dots and spaces, a
//!   `:` stream suffix and default-ignorable code points dropped, any case,
//!   git's 8.3 short name `git~<n>`). git checks again: the index commands
//!   run with `core.protectNTFS` and `core.protectHFS`, and a path git
//!   refuses is `FG-GIT-0028 rule=refused_by_git`, never a 500.
//! - [`check_content`]: at most [`MAX_FILE_BYTES`] (or the caller's lower
//!   limit, `FG-GIT-0029`), no NUL (`FG-GIT-0030`).
//! - [`Git::file_entry`]: what the tree of a commit holds at the path —
//!   `ls-tree -t -z --full-tree <commit> -- <path>` (the trees on the way
//!   are listed with `-t`) and, when the path is absent, one more `ls-tree`
//!   of the first missing segment (a file there is `parent_is_file`). A
//!   directory, a symbolic link or a submodule at the path is
//!   `FG-GIT-0028`.
//! - [`Git::commit_exists`]: `cat-file --batch-check` of an id on stdin.
//! - [`Git::write_blob`]: `hash-object -w --stdin --no-filters` (the
//!   content on stdin).
//! - [`Git::write_file_commit`]: a temporary index under
//!   `<git home>/file-commits/<uuid>.index` (`GIT_INDEX_FILE`; removed on
//!   every way out): `read-tree --end-of-options <parent>`, `update-index
//!   --add --index-info` (the entry on stdin, never in argv), `write-tree`,
//!   then [`Git::commit_tree`] with the parent and the identities. Index
//!   files left by a killed server are swept on each call (older than
//!   [`STALE_INDEX_AGE`]).
//!
//! - [`Git::commit_one_file`]: the whole commit for the editor route — the
//!   head read, the "no silent overwrite" check against the start commit,
//!   the writes above and ONE compare-and-swap `update-ref`, retried when a
//!   concurrent push wins, all within one overall deadline
//!   ([`COMMIT_DEADLINE`]; the swap itself is never cut short, so its
//!   outcome is always known). The start commit must be an ancestor of the
//!   head. The caller decides the ref update like a push before calling,
//!   and records the push after.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use forge_core::{ObjectId, RefName, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use crate::command::Git;
use crate::commit::{CommitIdentity, NewCommit};
use crate::refs::RefChange;
use crate::revision::Revision;
use crate::storage::StorageRoot;

/// The largest file the editor writes: the push's secret scan reads a text
/// blob whole up to this size (`push_scan::SECRET_BLOB_MAX`), and the
/// browse API shows a file as text up to it.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
/// The longest path (bytes), as the browse API's `RepoPath`.
pub const MAX_PATH_BYTES: usize = 4096;
/// `ls-tree` of one path prints a few lines; anything past this is not git's
/// answer to the question.
const LS_TREE_OUTPUT_BYTES: usize = 64 * 1024;
/// The temporary indexes' directory under the git home.
const INDEX_DIR: &str = "file-commits";
/// An index file older than this was left by a killed server: swept.
pub const STALE_INDEX_AGE: Duration = Duration::from_secs(3600);
/// The bound of one whole [`Git::commit_one_file`] (every attempt).
pub const COMMIT_DEADLINE: Duration = Duration::from_secs(20);

fn path_invalid(rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::FileCommitPathInvalid).with_context("rule", rule)
}

/// A path the editor may write (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePath(String);

impl FilePath {
    /// `FG-GIT-0028` with `rule` for a path breaking a rule (the value is
    /// never in the error).
    pub fn parse(raw: &str) -> Result<Self, ForgeError> {
        if raw.is_empty() {
            return Err(path_invalid("empty"));
        }
        if raw.len() > MAX_PATH_BYTES {
            return Err(path_invalid("too_long"));
        }
        if raw.starts_with('/') {
            return Err(path_invalid("absolute"));
        }
        if raw.chars().any(char::is_control) {
            return Err(path_invalid("control"));
        }
        if raw.contains('\\') {
            return Err(path_invalid("backslash"));
        }
        for segment in raw.split('/') {
            if segment.is_empty() {
                return Err(path_invalid("empty_segment"));
            }
            if segment == "." || segment == ".." {
                return Err(path_invalid("dot_segment"));
            }
            // As NTFS / HFS+ read the name (review E1): `.git.`, `.git::$DATA`,
            // `.g<U+200B>it`, `.GIT ` and git's 8.3 short name `git~1` are
            // all git's own directory on a developer's PC.
            let disk = forge_core::risk::on_disk_upper(segment);
            if disk.is_empty() {
                return Err(path_invalid("dot_segment"));
            }
            let short_name = disk
                .strip_prefix("GIT~")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            if disk == ".GIT" || short_name {
                return Err(path_invalid("dot_git"));
            }
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Every leading part of the path, the path itself last
    /// (`a`, `a/b`, `a/b/c.md`).
    fn prefixes(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self
            .0
            .match_indices('/')
            .filter_map(|(i, _)| self.0.get(..i))
            .collect();
        out.push(&self.0);
        out
    }
}

/// `FG-GIT-0029` past `limit` (the lower of [`MAX_FILE_BYTES`] and the
/// caller's), `FG-GIT-0030` for a NUL.
pub fn check_content(content: &str, limit: usize) -> Result<(), ForgeError> {
    let limit = limit.min(MAX_FILE_BYTES);
    if content.len() > limit {
        return Err(ForgeError::new(ErrorCode::FileCommitTooLarge)
            .with_context("bytes", content.len())
            .with_context("limit", limit));
    }
    if content.as_bytes().contains(&0) {
        return Err(ForgeError::new(ErrorCode::FileCommitNotText).with_context("reason", "nul"));
    }
    Ok(())
}

/// A regular file's mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    Regular,
    Executable,
}

impl FileMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Regular => "100644",
            Self::Executable => "100755",
        }
    }
}

/// The file a commit's tree holds at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub mode: FileMode,
    pub blob: ObjectId,
}

/// What one commit is made of (module docs).
#[derive(Debug, Clone)]
pub struct FileCommit<'a> {
    pub parent: &'a ObjectId,
    pub path: &'a FilePath,
    pub blob: &'a ObjectId,
    pub mode: FileMode,
    pub message: &'a str,
    pub author: &'a CommitIdentity,
    pub committer: &'a CommitIdentity,
}

/// One `ls-tree -z` line: `<mode> <type> <oid>\t<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Listed {
    mode: String,
    kind: String,
    oid: String,
    name: String,
}

fn unexpected(subcommand: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed)
        .with_context("subcommand", subcommand)
        .with_context("reason", "unexpected_output")
}

fn parse_ls_tree(out: &[u8]) -> Result<Vec<Listed>, ForgeError> {
    let mut entries = Vec::new();
    for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let text = std::str::from_utf8(record).map_err(|_| unexpected("ls-tree"))?;
        let (meta, name) = text.split_once('\t').ok_or_else(|| unexpected("ls-tree"))?;
        let mut parts = meta.split(' ');
        let (Some(mode), Some(kind), Some(oid), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(unexpected("ls-tree"));
        };
        entries.push(Listed {
            mode: mode.to_owned(),
            kind: kind.to_owned(),
            oid: oid.to_owned(),
            name: name.to_owned(),
        });
    }
    Ok(entries)
}

/// Removes the temporary index (and git's lock of it) on every way out.
struct TempIndex(PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        for path in [self.0.clone(), self.0.with_extension("index.lock")] {
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        target: "forge_git",
                        event = "forge.git.file_commit_index_left",
                        error_code = ErrorCode::StorageIoFailed.id(),
                        error = %e,
                        "a temporary index could not be removed"
                    );
                }
            }
        }
    }
}

/// Remove the index files (and their locks) in `dir` older than `max_age`
/// (a server killed mid-commit leaves one). Logged once when any was
/// removed; a failure to read or remove is logged, never fatal. The number
/// removed.
pub(crate) fn sweep_stale_indexes(dir: &Path, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0; // not created yet: nothing to sweep
    };
    let now = SystemTime::now();
    let (mut removed, mut failed) = (0usize, 0usize);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.ends_with(".index") || name.ends_with(".index.lock")) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > max_age);
        if !old {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(_) => failed += 1,
        }
    }
    if removed > 0 || failed > 0 {
        tracing::info!(
            target: "forge_git",
            event = "forge.git.file_commit_index_swept",
            removed,
            failed,
            "stale temporary indexes of the simple editor were removed"
        );
    }
    removed
}

impl Git {
    /// `ls-tree -z [-t] --full-tree <commit> -- <path>` of one repository.
    async fn ls_tree(
        &self,
        repo: &std::path::Path,
        commit: &ObjectId,
        path: &str,
        with_trees: bool,
        timeout: Duration,
    ) -> Result<Vec<Listed>, ForgeError> {
        let mut cmd = self.command("ls-tree").arg("-z");
        if with_trees {
            cmd = cmd.arg("-t");
        }
        let (out, cut) = cmd
            .arg("--full-tree")
            .revision(Revision::object_id(commit.as_str())?)
            .path(path)
            .repo(repo)
            .timeout(timeout)
            .output_truncated(LS_TREE_OUTPUT_BYTES)
            .await?;
        if cut {
            return Err(unexpected("ls-tree").with_context("detail", "truncated"));
        }
        parse_ls_tree(&out.stdout)
    }

    /// The regular file at `path` in `commit` (`None`: nothing there, nor
    /// a file on the way). `FG-GIT-0028` (`path_is_directory`,
    /// `parent_is_file`, `not_regular_file`) for anything else there.
    pub async fn file_entry(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        commit: &ObjectId,
        path: &FilePath,
        timeout: Duration,
    ) -> Result<Option<FileEntry>, ForgeError> {
        let repo = storage.existing(id)?;
        let listed = self
            .ls_tree(&repo, commit, path.as_str(), true, timeout)
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        let prefixes = path.prefixes();
        if let Some(entry) = listed.iter().find(|e| e.name == path.as_str()) {
            return match (entry.mode.as_str(), entry.kind.as_str()) {
                ("100644", "blob") => Ok(Some(FileEntry {
                    mode: FileMode::Regular,
                    blob: ObjectId::parse(&entry.oid).map_err(|_| unexpected("ls-tree"))?,
                })),
                ("100755", "blob") => Ok(Some(FileEntry {
                    mode: FileMode::Executable,
                    blob: ObjectId::parse(&entry.oid).map_err(|_| unexpected("ls-tree"))?,
                })),
                (_, "tree") => Err(path_invalid("path_is_directory")),
                _ => Err(path_invalid("not_regular_file")),
            };
        }
        // `-t` lists the trees on the way: the first part not listed as a
        // tree is where the path stops existing.
        let Some(first_missing) = prefixes.iter().find(|p| {
            !listed
                .iter()
                .any(|e| e.name == **p && e.kind.as_str() == "tree")
        }) else {
            return Ok(None);
        };
        if *first_missing == path.as_str() {
            return Ok(None);
        }
        // Not a tree there: a file (or a link, a submodule) blocks the path.
        let there = self
            .ls_tree(&repo, commit, first_missing, false, timeout)
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        if there.iter().any(|e| e.name == *first_missing) {
            return Err(path_invalid("parent_is_file"));
        }
        Ok(None)
    }

    /// Whether `oid` names a commit of repository `id` (`cat-file
    /// --batch-check`, the id on stdin).
    pub async fn commit_exists(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        oid: &ObjectId,
        timeout: Duration,
    ) -> Result<bool, ForgeError> {
        let repo = storage.existing(id)?;
        let out = self
            .command("cat-file")
            .arg("--batch-check=%(objecttype)")
            .stdin(format!("{}\n", oid.as_str()).into_bytes())
            .repo(&repo)
            .timeout(timeout)
            .output()
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.trim();
        if line == "commit" {
            return Ok(true);
        }
        if line.ends_with(" missing") || matches!(line, "blob" | "tree" | "tag") {
            return Ok(false);
        }
        Err(unexpected("cat-file").with_context("repo_id", id.get()))
    }

    /// Write `content` as a blob of repository `id`; its id.
    pub async fn write_blob(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        content: &[u8],
        timeout: Duration,
    ) -> Result<ObjectId, ForgeError> {
        let repo = storage.existing(id)?;
        let out = self
            .command("hash-object")
            .arg("-w")
            .arg("--stdin")
            .arg("--no-filters")
            .stdin(content.to_vec())
            .repo(&repo)
            .timeout(timeout)
            .output()
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        ObjectId::parse(String::from_utf8_lossy(&out.stdout).trim())
            .map_err(|_| unexpected("hash-object"))
    }

    /// Write the commit of `commit` (module docs); its id.
    pub async fn write_file_commit(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        commit: &FileCommit<'_>,
        timeout: Duration,
    ) -> Result<ObjectId, ForgeError> {
        let repo = storage.existing(id)?;
        let dir = self.config().home.join(INDEX_DIR);
        std::fs::create_dir_all(&dir).map_err(|e| {
            ForgeError::new(ErrorCode::StorageIoFailed)
                .with_context("operation", "file_commit_index_dir")
                .with_source(e)
        })?;
        sweep_stale_indexes(&dir, STALE_INDEX_AGE);
        let index = TempIndex(dir.join(format!("{}.index", uuid::Uuid::new_v4())));
        let index_env = OsString::from(index.0.as_os_str());
        let with_index = |sub: &'static str| {
            self.command(sub)
                // git refuses `.git` look-alikes of NTFS and HFS+ itself too.
                .config_override("core.protectNTFS", "true")
                .config_override("core.protectHFS", "true")
                .env("GIT_INDEX_FILE", index_env.clone()) // clone: each command owns its environment
                .repo(&repo)
                .timeout(timeout)
        };
        let fail = |e: ForgeError| e.with_context("repo_id", id.get());
        with_index("read-tree")
            .revision(Revision::object_id(commit.parent.as_str())?)
            .output()
            .await
            .map_err(fail)?;
        let line = format!(
            "{} blob {}\t{}\n",
            commit.mode.as_str(),
            commit.blob.as_str(),
            commit.path.as_str()
        );
        with_index("update-index")
            .arg("--add")
            .arg("--index-info")
            .stdin(line.into_bytes())
            .output()
            .await
            .map_err(|e| {
                // The entry was checked: git refusing it is the path's fault
                // (a 4xx), the cause kept for the log.
                if e.code() == ErrorCode::GitProcessFailed {
                    path_invalid("refused_by_git")
                        .with_context("repo_id", id.get())
                        .with_source(e)
                } else {
                    fail(e)
                }
            })?;
        let out = with_index("write-tree").output().await.map_err(fail)?;
        let tree = ObjectId::parse(String::from_utf8_lossy(&out.stdout).trim())
            .map_err(|_| unexpected("write-tree"))?;
        drop(index);
        // git skips a path it refuses ("Ignoring path …") and still exits 0:
        // the tree must hold exactly the entry, or the commit would be empty.
        let written = self
            .ls_tree(&repo, &tree, commit.path.as_str(), false, timeout)
            .await
            .map_err(fail)?;
        let held = written.iter().any(|e| {
            e.name == commit.path.as_str()
                && e.kind == "blob"
                && e.mode == commit.mode.as_str()
                && e.oid == commit.blob.as_str()
        });
        if !held {
            return Err(path_invalid("refused_by_git").with_context("repo_id", id.get()));
        }
        self.commit_tree(
            storage,
            id,
            &NewCommit {
                tree: &tree,
                parents: std::slice::from_ref(commit.parent),
                message: commit.message,
                author: commit.author,
                committer: commit.committer,
            },
            timeout,
        )
        .await
    }
}

impl Git {
    /// Whether another ref takes `name`'s place in git's ref namespace: a
    /// ref at one of its parent paths (`refs/heads/edit` for
    /// `refs/heads/edit/x`), or refs below it (`refs/heads/edit/x/y`).
    /// `name` itself existing is not this (that is `branch_exists`).
    pub async fn ref_name_taken(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        name: &RefName,
        timeout: Duration,
    ) -> Result<bool, ForgeError> {
        let below = self.read_refs(storage, id, name, timeout).await?;
        if below.keys().any(|r| r != name) {
            return Ok(true);
        }
        let full = name.as_str();
        // `refs/heads/a/b/c` -> `refs/heads/a`, `refs/heads/a/b`.
        let base = full
            .match_indices('/')
            .nth(1)
            .map_or(full.len(), |(i, _)| i);
        for (i, _) in full.match_indices('/').filter(|(i, _)| *i > base) {
            let Some(parent) = full.get(..i) else {
                continue;
            };
            let parent = RefName::parse(parent)?;
            if self
                .read_ref(storage, id, &parent, timeout)
                .await?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Builds of the commit when a concurrent push wins the compare-and-swap.
pub const ATTEMPTS: u32 = 3;

/// One file committed on a branch, or on a new branch made from it
/// ([`Git::commit_one_file`]).
#[derive(Debug, Clone)]
pub struct OneFile<'a> {
    /// The branch the file was read from (it must exist).
    pub branch: &'a RefName,
    /// The ref to move: `branch` itself, or the new branch to create.
    pub target: &'a RefName,
    /// `target` is created (it must not exist), else `branch` moves.
    pub create: bool,
    /// The commit the file was read at.
    pub start: &'a ObjectId,
    pub path: &'a FilePath,
    pub content: &'a [u8],
    pub message: &'a str,
    pub author: &'a CommitIdentity,
}

/// What [`Git::commit_one_file`] wrote and moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneFileCommitted {
    /// The branch's head the commit was built on.
    pub parent: ObjectId,
    pub commit: ObjectId,
    pub blob: ObjectId,
    pub attempts: u32,
}

/// The overall deadline of one commit: the time left, or `FG-GIT-0002
/// operation=single_file_commit` once it passed.
struct Deadline {
    end: tokio::time::Instant,
    total: Duration,
}

impl Deadline {
    fn new(total: Duration) -> Self {
        Self {
            end: tokio::time::Instant::now() + total,
            total,
        }
    }

    /// `step` bounded by the time left.
    fn bound(&self, step: Duration) -> Result<Duration, ForgeError> {
        let left = self
            .end
            .saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(ForgeError::new(ErrorCode::GitTimeout)
                .with_context("operation", "single_file_commit")
                .with_context("timeout_ms", self.total.as_millis()));
        }
        Ok(step.min(left))
    }
}

fn conflict(reason: &'static str, start: &ObjectId, head: Option<&ObjectId>) -> ForgeError {
    ForgeError::new(ErrorCode::FileCommitConflict)
        .with_context("reason", reason)
        .with_context("start_sha", start.as_str().to_owned())
        .with_context("head_sha", head.map_or("", ObjectId::as_str).to_owned())
}

impl Git {
    /// Commit `one` and move its ref with ONE compare-and-swap `update-ref`
    /// (module docs): the head is read; when it is not `start` the file at
    /// both must be the same (`FG-GIT-0027 reason=file_changed`, or
    /// `start_unknown`); the same content is `FG-GIT-0031`; a lost swap is
    /// read again and the build retried ([`ATTEMPTS`] in all, then
    /// `FG-GIT-0027 reason=ref_moved`); an existing new branch is
    /// `reason=branch_exists`. A missing `branch` is `FG-REPO-0023
    /// reason=ref_not_found`; a start that is not an ancestor of the head
    /// `FG-GIT-0027 reason=start_not_ancestor`. `ref_written` runs after
    /// every `update-ref` (whatever its outcome: the caller drops its ref
    /// caches). Each git step has `timeout`, and all of them `total`
    /// (`FG-GIT-0002 operation=single_file_commit` past it) — except the
    /// swap and the read that tells its outcome, which always get `timeout`
    /// once started. The caller decided the ref update like a push before
    /// calling.
    pub async fn commit_one_file(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        one: &OneFile<'_>,
        (timeout, total): (Duration, Duration),
        ref_written: &(dyn Fn() + Send + Sync),
    ) -> Result<OneFileCommitted, ForgeError> {
        let deadline = Deadline::new(total);
        // A new branch whose name a ref's directory or file takes (`edit/x`
        // while `edit` exists, or the reverse): the user's naming, a 409.
        if one.create
            && self
                .ref_name_taken(storage, id, one.target, deadline.bound(timeout)?)
                .await?
        {
            return Err(conflict("branch_name_conflict", one.start, None));
        }
        let blob = self
            .write_blob(storage, id, one.content, deadline.bound(timeout)?)
            .await?;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let head = self
                .read_ref(storage, id, one.branch, deadline.bound(timeout)?)
                .await?
                .ok_or_else(|| {
                    ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                        .with_context("reason", "ref_not_found")
                        .with_context("param", "branch")
                })?;
            let at_head = self
                .file_entry(storage, id, &head, one.path, deadline.bound(timeout)?)
                .await?;
            if head != *one.start {
                if !self
                    .commit_exists(storage, id, one.start, deadline.bound(timeout)?)
                    .await?
                {
                    return Err(conflict("start_unknown", one.start, Some(&head)));
                }
                if !self
                    .is_ancestor(storage, id, one.start, &head, deadline.bound(timeout)?)
                    .await?
                {
                    return Err(conflict("start_not_ancestor", one.start, Some(&head)));
                }
                // A path that was something else at the start changed too.
                let at_start = self
                    .file_entry(storage, id, one.start, one.path, deadline.bound(timeout)?)
                    .await
                    .map_err(|e| {
                        if e.code() == ErrorCode::FileCommitPathInvalid {
                            conflict("file_changed", one.start, Some(&head))
                        } else {
                            e
                        }
                    })?;
                if at_start != at_head {
                    return Err(conflict("file_changed", one.start, Some(&head)));
                }
            }
            let mode = at_head.as_ref().map_or(FileMode::Regular, |e| e.mode);
            if at_head.as_ref().is_some_and(|e| e.blob == blob) {
                return Err(ForgeError::new(ErrorCode::FileCommitUnchanged)
                    .with_context("blob", blob.as_str().to_owned()));
            }
            let commit = self
                .write_file_commit(
                    storage,
                    id,
                    &FileCommit {
                        parent: &head,
                        path: one.path,
                        blob: &blob,
                        mode,
                        message: one.message,
                        author: one.author,
                        committer: one.author,
                    },
                    deadline.bound(timeout)?,
                )
                .await?;
            // Not started past the deadline; once started, never cut short.
            deadline.bound(timeout)?;
            let change = if one.create {
                RefChange::Create {
                    name: one.target.clone(), // clone: the change owns its name
                    new: commit.clone(),      // clone: as above
                }
            } else {
                RefChange::Update {
                    name: one.target.clone(), // clone: as above
                    new: commit.clone(),      // clone: as above
                    old: head.clone(),        // clone: as above
                }
            };
            let moved = self.update_refs(storage, id, &[change], timeout).await;
            ref_written();
            let Err(cause) = moved else {
                return Ok(OneFileCommitted {
                    parent: head,
                    commit,
                    blob,
                    attempts,
                });
            };
            // A lost compare-and-swap is told apart by reading the ref again.
            let now = self.read_ref(storage, id, one.target, timeout).await?;
            if now.as_ref() == Some(&commit) {
                return Ok(OneFileCommitted {
                    parent: head,
                    commit,
                    blob,
                    attempts,
                });
            }
            if one.create && now.is_some() {
                return Err(conflict("branch_exists", one.start, now.as_ref())
                    .with_context("cause_code", cause.code().id()));
            }
            // A conflicting ref created meanwhile (the check above raced).
            if one.create
                && self
                    .ref_name_taken(storage, id, one.target, timeout)
                    .await?
            {
                return Err(conflict("branch_name_conflict", one.start, None)
                    .with_context("cause_code", cause.code().id()));
            }
            let lost = !one.create && now.as_ref() != Some(&head);
            let locked = self.ref_locked(storage, id, one.target)?;
            if !(lost || locked) {
                return Err(cause);
            }
            if attempts >= ATTEMPTS {
                return Err(conflict("ref_moved", one.start, now.as_ref())
                    .with_context("attempts", attempts)
                    .with_context("cause_code", cause.code().id()));
            }
            tracing::info!(
                target: "forge_git",
                event = "forge.git.single_file_commit_retry",
                repo_id = id.get(),
                attempts,
                locked,
                "the branch moved during the commit; building it again"
            );
        }
    }
}

#[cfg(test)]
#[path = "file_commit_tests.rs"]
mod tests;
