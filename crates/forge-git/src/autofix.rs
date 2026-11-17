//! The git side of the deterministic autofix (M4 C14, ADR-0017 §1 ③④
//! and §1.2, Amendment C14). The runner's output is a git bundle of ONE
//! transport commit on the input commit; forge uses only its tree, builds
//! the bot's commit itself (`commit-tree`) and checks what changed.
//!
//! - [`bundle_header`]: the bundle's header (v2 / v3), parsed with bounds
//!   before git reads it: exactly one ref, [`forge_core::autofix::BUNDLE_REF`],
//!   and exactly the prerequisites forge expects (the input commit).
//! - [`Git::import_autofix_bundle`]: the size bound, the header, then
//!   `fetch` of the bundle with `fetch.fsckObjects` / `transfer.fsckObjects`
//!   and no ref written (`--no-write-fetch-head`, no destination: the
//!   objects stay unreferenced, pruned with the trial merges' grace), then
//!   the transport commit's parents must be exactly the input commit. Every
//!   refusal is `FG-GIT-0037` (alert: a runner defect or a compromise).
//! - [`Git::tree_changes`]: `diff-tree -r --no-renames --raw -z` of two
//!   trees, with modes and object ids, capped.
//! - [`Git::blob_at_most`]: one blob, `None` past the cap.
//! - [`Git::conflict_merge`]: `merge-tree --write-tree` that keeps the
//!   conflicted tree (the files with their conflict blocks) — the server's
//!   own computation the candidate is compared with (§1.2).
//! - [`Git::root_entry`] / [`Git::tree_with_root_entry`]: one entry of a
//!   tree's root (`ls-tree`), and the same tree with that entry replaced or
//!   removed (`mktree`): the input commit carries the target branch's
//!   `.forge-ci.yml`, and the bot's tree gets the base's back.
//!
//! Object ids reach git validated (after `--end-of-options` or on stdin);
//! the bundle path is forge's own. Every call has the caller's timeout.

use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_core::autofix::{ChangeKind, BUNDLE_REF};
use forge_core::{ObjectId, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use crate::command::Git;
use crate::mergeability::MergeTreeLimits;
use crate::revision::Revision;
use crate::storage::StorageRoot;

/// The most header bytes read from a bundle.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// `cat-file commit` of one commit, `ls-tree` of a root, a tree id.
const SMALL_OUTPUT_BYTES: usize = 1024 * 1024;

/// `FG-GIT-0037 AutofixCandidateInvalid` with `reason`.
pub fn candidate_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::AutofixCandidateInvalid).with_context("reason", reason)
}

/// What a bundle's header says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleHeader {
    /// The commits the bundle needs (`-<id>` lines).
    pub prerequisites: Vec<ObjectId>,
    /// `(<id>, <ref>)` lines.
    pub refs: Vec<(ObjectId, String)>,
}

/// The header of a bundle (`# v2 git bundle` or `# v3 git bundle` with
/// `@object-format=sha1|sha256` / `@filter` capabilities), up to the empty
/// line. `Err(reason)` for anything else.
pub fn bundle_header(bytes: &[u8]) -> Result<BundleHeader, &'static str> {
    let end = bytes
        .windows(2)
        .position(|w| w == b"\n\n")
        .ok_or("bundle_header_unterminated")?;
    let text = std::str::from_utf8(&bytes[..end]).map_err(|_| "bundle_header_not_utf8")?;
    let mut lines = text.split('\n');
    let v3 = match lines.next() {
        Some("# v2 git bundle") => false,
        Some("# v3 git bundle") => true,
        _ => return Err("bundle_signature"),
    };
    let mut header = BundleHeader {
        prerequisites: Vec::new(),
        refs: Vec::new(),
    };
    for line in lines {
        if let Some(cap) = line.strip_prefix('@') {
            // Only v3 has capabilities; a filter (a partial bundle) is
            // never accepted: every object must be in it.
            match (v3, cap) {
                (true, "object-format=sha1" | "object-format=sha256") => continue,
                _ => return Err("bundle_capability"),
            }
        }
        if let Some(rest) = line.strip_prefix('-') {
            let id = rest.split(' ').next().unwrap_or("");
            header
                .prerequisites
                .push(ObjectId::parse(id).map_err(|_| "bundle_prerequisite")?);
            continue;
        }
        let (id, name) = line.split_once(' ').ok_or("bundle_ref_line")?;
        let id = ObjectId::parse(id).map_err(|_| "bundle_ref_line")?;
        header.refs.push((id, name.to_owned()));
    }
    Ok(header)
}

/// The bounds of one import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportLimits {
    /// The bundle file.
    pub max_bytes: u64,
    /// One blob the bundle brings, unpacked.
    pub max_blob_bytes: u64,
    /// Every object the bundle brings, unpacked, together.
    pub max_unpacked_bytes: u64,
    pub timeout: Duration,
}

/// What an import found.
#[derive(Debug)]
pub struct ImportedCandidate {
    /// The transport commit (never on a ref; only its tree is used).
    pub transport: ObjectId,
    /// The candidate tree (in the quarantine).
    pub tree: ObjectId,
    /// Where its objects are until [`Git::migrate_quarantine`].
    pub quarantine: Quarantine,
}

/// One changed path between two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeChange {
    pub path: String,
    pub kind: ChangeKind,
    pub old_mode: u32,
    pub new_mode: u32,
    /// `None` on the absent side.
    pub old_id: Option<ObjectId>,
    pub new_id: Option<ObjectId>,
}

/// The changed paths between two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeChanges {
    Complete(Vec<TreeChange>),
    /// More than the cap.
    TooMany,
    /// A path that is not UTF-8.
    NonUtf8,
}

/// The server's own merge of a conflicting merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictMerge {
    /// No conflict (any more).
    Clean { tree: ObjectId },
    /// The tree `merge-tree` wrote (conflicted files with their blocks)
    /// and the conflicted paths (`files_truncated`: more exist or the
    /// output was cut).
    Conflicted {
        tree: ObjectId,
        files: Vec<String>,
        files_truncated: bool,
    },
}

/// One root entry of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootEntry {
    /// `100644`, `100755`, `120000`, `040000`, `160000`.
    pub mode: String,
    /// `blob`, `tree`, `commit`.
    pub kind: String,
    pub id: ObjectId,
}

fn unexpected(subcommand: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitProcessFailed)
        .with_context("subcommand", subcommand)
        .with_context("reason", reason)
}

fn status_kind(status: &str) -> ChangeKind {
    match status.chars().next() {
        Some('A') => ChangeKind::Added,
        Some('M') => ChangeKind::Modified,
        Some('D') => ChangeKind::Deleted,
        Some(_) | None => ChangeKind::Other,
    }
}

fn zero_to_none(id: &str) -> Result<Option<ObjectId>, ForgeError> {
    if id.bytes().all(|b| b == b'0') {
        return Ok(None);
    }
    ObjectId::parse(id)
        .map(Some)
        .map_err(|_| unexpected("diff-tree", "unexpected_output"))
}

/// Parse `diff-tree -r --no-renames --raw -z`: `:<old> <new> <old id>
/// <new id> <status>` NUL `<path>` NUL per entry.
pub(crate) fn parse_raw(stdout: &[u8], max: usize) -> Result<TreeChanges, ForgeError> {
    let mut pieces: Vec<&[u8]> = stdout.split(|b| *b == 0).collect();
    pieces.pop();
    let mut it = pieces.into_iter();
    let mut out = Vec::new();
    while let Some(record) = it.next() {
        let Ok(header) = std::str::from_utf8(record) else {
            return Ok(TreeChanges::NonUtf8);
        };
        let header = header
            .strip_prefix(':')
            .ok_or_else(|| unexpected("diff-tree", "unexpected_output"))?;
        let path = it
            .next()
            .ok_or_else(|| unexpected("diff-tree", "raw_without_path"))?;
        let Ok(path) = std::str::from_utf8(path) else {
            return Ok(TreeChanges::NonUtf8);
        };
        let fields: Vec<&str> = header.split(' ').collect();
        let [old_mode, new_mode, old_id, new_id, status] = fields.as_slice() else {
            return Err(unexpected("diff-tree", "unexpected_output"));
        };
        if out.len() >= max {
            return Ok(TreeChanges::TooMany);
        }
        let mode = |m: &str| {
            u32::from_str_radix(m, 8).map_err(|_| unexpected("diff-tree", "unexpected_output"))
        };
        out.push(TreeChange {
            path: path.to_owned(),
            kind: status_kind(status),
            old_mode: mode(old_mode)?,
            new_mode: mode(new_mode)?,
            old_id: zero_to_none(old_id)?,
            new_id: zero_to_none(new_id)?,
        });
    }
    Ok(TreeChanges::Complete(out))
}

/// The parents and tree of a `cat-file commit` body.
fn commit_header(body: &[u8]) -> Result<(ObjectId, Vec<ObjectId>), ForgeError> {
    let text = String::from_utf8_lossy(body);
    let mut tree = None;
    let mut parents = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            break;
        }
        if let Some(id) = line.strip_prefix("tree ") {
            tree = Some(ObjectId::parse(id).map_err(|_| unexpected("cat-file", "commit_shape"))?);
        } else if let Some(id) = line.strip_prefix("parent ") {
            parents.push(ObjectId::parse(id).map_err(|_| unexpected("cat-file", "commit_shape"))?);
        }
    }
    Ok((
        tree.ok_or_else(|| unexpected("cat-file", "commit_shape"))?,
        parents,
    ))
}

/// The tree `merge-tree --write-tree --name-only -z` printed on exit 1,
/// and its conflicted paths (as `mergeability::conflicted_files`).
fn conflicted(stdout: &[u8], cut: bool, max_files: usize) -> Result<ConflictMerge, ForgeError> {
    let mut pieces: Vec<&[u8]> = stdout.split(|b| *b == 0).collect();
    pieces.pop();
    let mut records = pieces.into_iter();
    let tree = records
        .next()
        .and_then(|t| ObjectId::parse(&String::from_utf8_lossy(t)).ok())
        .ok_or_else(|| unexpected("merge-tree", "unexpected_output"))?;
    let mut files = Vec::new();
    let (mut over, mut ended) = (false, false);
    for record in records {
        if record.is_empty() {
            ended = true;
            break;
        }
        if files.len() < max_files {
            files.push(String::from_utf8_lossy(record).into_owned());
        } else {
            over = true;
        }
    }
    if !ended && !cut {
        return Err(unexpected("merge-tree", "unexpected_output"));
    }
    Ok(ConflictMerge::Conflicted {
        tree,
        files,
        files_truncated: over || !ended,
    })
}

impl Git {
    /// Import the runner's bundle at `bundle` into a QUARANTINE of
    /// repository `id` (module docs): the bundle is copied into a fresh
    /// object directory under the repository's `objects/` while its sha256
    /// is computed (it must be `expected_sha256`: the file is read once);
    /// the header is checked (`input` is the only prerequisite allowed);
    /// `fetch` writes the objects into the quarantine only
    /// (`GIT_OBJECT_DIRECTORY`, the repository's objects as alternates,
    /// fsck, empty hooks); every object the bundle brought is sized (no blob
    /// over `limits.max_blob_bytes`, no more than `limits.max_unpacked_bytes`
    /// in all: a delta bomb is refused here); the transport commit's only
    /// parent must be `input`. Nothing reaches the repository until
    /// [`Git::migrate_quarantine`]; dropping the quarantine removes it.
    pub async fn import_autofix_bundle(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        bundle: &Path,
        expected_sha256: &str,
        input: &ObjectId,
        limits: ImportLimits,
    ) -> Result<ImportedCandidate, ForgeError> {
        let repo = storage.existing(id)?;
        let quarantine = Quarantine::create(&repo).await?;
        let copy = quarantine.dir.join("autofix.bundle");
        let digest = copy_hashed(bundle, &copy, limits.max_bytes).await?;
        if digest != expected_sha256 {
            return Err(candidate_invalid("artifact_digest"));
        }
        let head = read_head(&copy).await?;
        let header = bundle_header(&head).map_err(candidate_invalid)?;
        if header.prerequisites.as_slice() != std::slice::from_ref(input) {
            return Err(candidate_invalid("precondition_mismatch")
                .with_context("prerequisites", header.prerequisites.len())
                .with_context("input", input.as_str().to_owned()));
        }
        let [(transport, name)] = header.refs.as_slice() else {
            return Err(candidate_invalid("bundle_refs").with_context("refs", header.refs.len()));
        };
        if name != BUNDLE_REF {
            return Err(candidate_invalid("bundle_refs"));
        }
        // The objects, checked by git (fsck), into the quarantine only; no
        // ref written, no hook run.
        let fetched = self
            .with_empty_hooks(self.command("fetch"))?
            .config_override("fetch.fsckObjects", "true")
            .config_override("transfer.fsckObjects", "true")
            .arg("--quiet")
            .arg("--no-tags")
            .arg("--no-write-fetch-head")
            .arg("--no-recurse-submodules")
            .end_operand(copy.as_os_str().to_os_string())
            .end_operand(BUNDLE_REF.into())
            .quarantined(Some(&quarantine))
            .repo(&repo)
            .timeout(limits.timeout)
            .output_accepting(SMALL_OUTPUT_BYTES, &[0, 1, 128])
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        if fetched.exit_code != Some(0) {
            return Err(candidate_invalid("bundle_rejected")
                .with_context("exit_code", fetched.exit_code.unwrap_or(-1))
                .with_context("stderr", fetched.stderr));
        }
        self.size_quarantine(&repo, &quarantine, limits).await?;
        let body = self
            .command("cat-file")
            .arg("commit")
            .revision(Revision::object_id(transport.as_str())?)
            .quarantined(Some(&quarantine))
            .repo(&repo)
            .timeout(limits.timeout)
            .output_truncated(SMALL_OUTPUT_BYTES)
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?
            .0;
        let (tree, parents) = commit_header(&body.stdout)?;
        if parents.as_slice() != std::slice::from_ref(input) {
            return Err(candidate_invalid("precondition_mismatch")
                .with_context("parents", parents.len())
                .with_context("input", input.as_str().to_owned()));
        }
        Ok(ImportedCandidate {
            transport: transport.clone(), // clone: the header is dropped
            tree,
            quarantine,
        })
    }

    /// Every object the bundle brought (the quarantine's own objects, the
    /// repository's not as alternates), sized: `FG-GIT-0037 blob_too_large`
    /// / `unpacked_too_large` / `too_many_objects`.
    async fn size_quarantine(
        &self,
        repo: &Path,
        q: &Quarantine,
        limits: ImportLimits,
    ) -> Result<(), ForgeError> {
        let (out, truncated) = self
            .command("cat-file")
            .arg("--batch-all-objects")
            .arg("--unordered")
            .arg("--batch-check=%(objecttype) %(objectsize)")
            .env("GIT_OBJECT_DIRECTORY", q.dir.as_os_str().to_os_string())
            .repo(repo)
            .timeout(limits.timeout)
            .output_truncated(SIZE_LISTING_BYTES)
            .await?;
        if truncated {
            return Err(candidate_invalid("too_many_objects"));
        }
        let mut total: u64 = 0;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let (kind, size) = line
                .split_once(' ')
                .ok_or_else(|| unexpected("cat-file", "unexpected_output"))?;
            let size: u64 = size
                .parse()
                .map_err(|_| unexpected("cat-file", "unexpected_output"))?;
            if kind == "blob" && size > limits.max_blob_bytes {
                return Err(candidate_invalid("blob_too_large")
                    .with_context("bytes", size)
                    .with_context("limit_bytes", limits.max_blob_bytes));
            }
            total = total.saturating_add(size);
            if total > limits.max_unpacked_bytes {
                return Err(candidate_invalid("unpacked_too_large")
                    .with_context("limit_bytes", limits.max_unpacked_bytes));
            }
        }
        Ok(())
    }

    /// The tree of commit `commit` (`cat-file commit`), read through the
    /// quarantine `q` when given.
    pub async fn tree_of_in(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        q: Option<&Quarantine>,
        commit: &ObjectId,
        timeout: Duration,
    ) -> Result<ObjectId, ForgeError> {
        let repo = storage.existing(id)?;
        let body = self
            .command("cat-file")
            .arg("commit")
            .revision(Revision::object_id(commit.as_str())?)
            .quarantined(q)
            .repo(&repo)
            .timeout(timeout)
            .output_truncated(SMALL_OUTPUT_BYTES)
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?
            .0;
        commit_header(&body.stdout).map(|(tree, _)| tree)
    }

    /// `commit-tree` for `commit`, written into the quarantine `q` when
    /// given (hooks pinned to the empty directory).
    pub async fn commit_in(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        q: Option<&Quarantine>,
        commit: &crate::commit::NewCommit<'_>,
        timeout: Duration,
    ) -> Result<ObjectId, ForgeError> {
        let repo = storage.existing(id)?;
        let out = self
            .commit_tree_command(&repo, commit)?
            .quarantined(q)
            .stdin(commit.message.as_bytes().to_vec())
            .timeout(timeout)
            .output()
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        ObjectId::parse(String::from_utf8_lossy(&out.stdout).trim())
            .map_err(|_| unexpected("commit-tree", "unexpected_output"))
    }

    /// Move the quarantine's objects into the repository (packs with their
    /// index last, then loose objects; one that exists already is kept)
    /// and remove the quarantine. Only after every check passed.
    pub async fn migrate_quarantine(&self, mut q: Quarantine) -> Result<(), ForgeError> {
        let objects = q.objects.clone(); // clone: q is consumed below
        let dir = q.dir.clone(); // clone: as above
        tokio::task::spawn_blocking(move || migrate_objects(&dir, &objects))
            .await
            .map_err(|e| {
                ForgeError::new(ErrorCode::StorageIoFailed)
                    .with_context("operation", "autofix_migrate")
                    .with_context(
                        "reason",
                        if e.is_panic() {
                            "panicked"
                        } else {
                            "cancelled"
                        },
                    )
            })??;
        q.migrated = true;
        tokio::fs::remove_dir_all(&q.dir).await.map_err(|e| {
            ForgeError::new(ErrorCode::StorageIoFailed)
                .with_context("operation", "autofix_quarantine_remove")
                .with_source(e)
        })
    }

    /// The paths changed from tree (or commit) `from` to `to`, at most
    /// `max` (module docs).
    pub async fn tree_changes(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        q: Option<&Quarantine>,
        (from, to): (&ObjectId, &ObjectId),
        max: usize,
        timeout: Duration,
    ) -> Result<TreeChanges, ForgeError> {
        let repo = storage.existing(id)?;
        let cap = max.saturating_add(1).saturating_mul(4096 + 256).max(8192);
        let (output, truncated) = self
            .command("diff-tree")
            .arg("-r")
            .arg("--no-renames")
            .arg("--raw")
            .arg("-z")
            .revision(Revision::object_id(from.as_str())?)
            .revision(Revision::object_id(to.as_str())?)
            .quarantined(q)
            .repo(&repo)
            .timeout(timeout)
            .output_truncated(cap)
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        if truncated {
            return Ok(TreeChanges::TooMany);
        }
        parse_raw(&output.stdout, max)
    }

    /// Blob `blob` of repository `id`; `None` when it is larger than
    /// `max_bytes`.
    pub async fn blob_at_most(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        q: Option<&Quarantine>,
        blob: &ObjectId,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, ForgeError> {
        let repo = storage.existing(id)?;
        let (output, truncated) = self
            .command("cat-file")
            .arg("blob")
            .revision(Revision::object_id(blob.as_str())?)
            .quarantined(q)
            .repo(&repo)
            .timeout(timeout)
            .output_truncated(max_bytes.saturating_add(1))
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        if truncated || output.stdout.len() > max_bytes {
            return Ok(None);
        }
        Ok(Some(output.stdout))
    }

    /// The server's merge of `head` into `target` keeping the conflicted
    /// tree (module docs). `FG-GIT-0003` for every exit but 0 and 1.
    pub async fn conflict_merge(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        target: &ObjectId,
        head: &ObjectId,
        limits: MergeTreeLimits,
    ) -> Result<ConflictMerge, ForgeError> {
        let repo = storage.existing(id)?;
        let captured = self
            .merge_tree_command(&repo, target, head)?
            .timeout(limits.timeout)
            .output_accepting(limits.max_output_bytes, &[0, 1])
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        let stdout = &captured.output.stdout;
        match captured.exit_code {
            Some(0) => {
                let text = String::from_utf8_lossy(stdout);
                let first = text.split(['\0', '\n']).next().unwrap_or("");
                Ok(ConflictMerge::Clean {
                    tree: ObjectId::parse(first.trim())
                        .map_err(|_| unexpected("merge-tree", "unexpected_output"))?,
                })
            }
            Some(1) | None => conflicted(stdout, captured.truncated, limits.max_files),
            Some(_) => Err(unexpected("merge-tree", "unaccepted_exit_code")),
        }
    }

    /// The root entry `name` of tree (or commit) `tree` (`None`: absent).
    /// `name` is forge's own constant (one path component).
    pub async fn root_entry(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        q: Option<&Quarantine>,
        tree: &ObjectId,
        name: &'static str,
        timeout: Duration,
    ) -> Result<Option<RootEntry>, ForgeError> {
        let repo = storage.existing(id)?;
        let out = self
            .command("ls-tree")
            .arg("-z")
            .revision(Revision::object_id(tree.as_str())?)
            .path(name)
            .quarantined(q)
            .repo(&repo)
            .timeout(timeout)
            .output_truncated(SMALL_OUTPUT_BYTES)
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?
            .0;
        for record in out.stdout.split(|b| *b == 0) {
            let text = String::from_utf8_lossy(record);
            let Some((meta, path)) = text.split_once('\t') else {
                continue;
            };
            if path != name {
                continue;
            }
            let mut parts = meta.split(' ');
            let (Some(mode), Some(kind), Some(oid)) = (parts.next(), parts.next(), parts.next())
            else {
                return Err(unexpected("ls-tree", "unexpected_output"));
            };
            return Ok(Some(RootEntry {
                mode: mode.to_owned(),
                kind: kind.to_owned(),
                id: ObjectId::parse(oid).map_err(|_| unexpected("ls-tree", "unexpected_output"))?,
            }));
        }
        Ok(None)
    }

    /// Tree `tree` with its root entry `name` replaced by `entry` (`None`:
    /// removed): `ls-tree -z` of the root, then `mktree -z`.
    #[allow(clippy::too_many_arguments)] // the repository, the quarantine and the edit
    pub async fn tree_with_root_entry(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        q: Option<&Quarantine>,
        tree: &ObjectId,
        name: &'static str,
        entry: Option<&RootEntry>,
        timeout: Duration,
    ) -> Result<ObjectId, ForgeError> {
        let repo = storage.existing(id)?;
        let listed = self
            .command("ls-tree")
            .arg("-z")
            .revision(Revision::object_id(tree.as_str())?)
            .quarantined(q)
            .repo(&repo)
            .timeout(timeout)
            .output()
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        let mut input: Vec<u8> = Vec::with_capacity(listed.stdout.len() + 128);
        for record in listed.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(|| unexpected("ls-tree", "unexpected_output"))?;
            if &record[tab + 1..] == name.as_bytes() {
                continue;
            }
            input.extend_from_slice(record);
            input.push(0);
        }
        if let Some(e) = entry {
            input.extend_from_slice(
                format!("{} {} {}\t{name}", e.mode, e.kind, e.id.as_str()).as_bytes(),
            );
            input.push(0);
        }
        let made = self
            .command("mktree")
            .arg("-z")
            .stdin(input)
            .quarantined(q)
            .repo(&repo)
            .timeout(timeout)
            .output()
            .await
            .map_err(|e| e.with_context("repo_id", id.get()))?;
        ObjectId::parse(String::from_utf8_lossy(&made.stdout).trim())
            .map_err(|_| unexpected("mktree", "unexpected_output"))
    }
}

/// The most bytes of the quarantine's size listing (one short line per
/// object: about 100 000 objects).
const SIZE_LISTING_BYTES: usize = 4 * 1024 * 1024;

/// A temporary object directory under the repository's `objects/` (the
/// same file system: the migration renames). Every command of an import's
/// checks runs with it as `GIT_OBJECT_DIRECTORY` and the repository's
/// objects as `GIT_ALTERNATE_OBJECT_DIRECTORIES`, as git's own push
/// quarantine. Dropped without [`Git::migrate_quarantine`], it is removed:
/// nothing a refused candidate brought stays in the repository.
#[derive(Debug)]
pub struct Quarantine {
    dir: PathBuf,
    objects: PathBuf,
    migrated: bool,
}

impl Quarantine {
    async fn create(repo: &Path) -> Result<Self, ForgeError> {
        let objects = repo.join("objects");
        let dir = objects.join(format!("forge-autofix-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir(&dir).await.map_err(|e| {
            ForgeError::new(ErrorCode::StorageIoFailed)
                .with_context("operation", "autofix_quarantine_create")
                .with_source(e)
        })?;
        Ok(Self {
            dir,
            objects,
            migrated: false,
        })
    }

    /// The quarantine's directory (tests).
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Quarantine {
    fn drop(&mut self) {
        if self.migrated {
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    target: "forge_git",
                    event = "forge.autofix.quarantine_left",
                    error_code = ErrorCode::StorageIoFailed.id(),
                    dir = %self.dir.display(),
                    error = %e,
                    "an autofix quarantine could not be removed (unreferenced objects only)"
                );
            }
        }
    }
}

impl crate::command::GitCommand {
    /// Run with quarantine `q` (when given) as the object directory and the
    /// repository's objects as alternates.
    #[must_use]
    pub(crate) fn quarantined(self, q: Option<&Quarantine>) -> Self {
        match q {
            None => self,
            Some(q) => self
                .env("GIT_OBJECT_DIRECTORY", q.dir.as_os_str().to_os_string())
                .env(
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    q.objects.as_os_str().to_os_string(),
                ),
        }
    }
}

fn storage_io(operation: &'static str, e: std::io::Error) -> ForgeError {
    ForgeError::new(ErrorCode::StorageIoFailed)
        .with_context("operation", operation)
        .with_source(e)
}

/// Rename `from` to `to` unless `to` exists (the same object).
fn place(from: &Path, to: &Path) -> Result<(), ForgeError> {
    if to.exists() {
        return Ok(());
    }
    std::fs::rename(from, to).map_err(|e| storage_io("autofix_migrate", e))
}

/// The migration's file moves (module docs of [`Git::migrate_quarantine`]).
fn migrate_objects(dir: &Path, objects: &Path) -> Result<(), ForgeError> {
    let pack = dir.join("pack");
    if pack.is_dir() {
        let target = objects.join("pack");
        std::fs::create_dir_all(&target).map_err(|e| storage_io("autofix_migrate", e))?;
        let mut files: Vec<PathBuf> = std::fs::read_dir(&pack)
            .map_err(|e| storage_io("autofix_migrate", e))?
            .map(|e| {
                e.map(|e| e.path())
                    .map_err(|e| storage_io("autofix_migrate", e))
            })
            .collect::<Result<_, _>>()?;
        // The index last: git uses a pack once its .idx is there.
        files.sort_by_key(|p| p.extension().is_some_and(|x| x == "idx"));
        for f in files {
            if let Some(name) = f.file_name() {
                place(&f, &target.join(name))?;
            }
        }
    }
    for entry in std::fs::read_dir(dir).map_err(|e| storage_io("autofix_migrate", e))? {
        let entry = entry.map_err(|e| storage_io("autofix_migrate", e))?;
        let name = entry.file_name();
        let loose = name.len() == 2
            && name
                .to_str()
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_hexdigit()));
        if !loose {
            continue;
        }
        let target = objects.join(&name);
        std::fs::create_dir_all(&target).map_err(|e| storage_io("autofix_migrate", e))?;
        for obj in std::fs::read_dir(entry.path()).map_err(|e| storage_io("autofix_migrate", e))? {
            let obj = obj.map_err(|e| storage_io("autofix_migrate", e))?;
            place(&obj.path(), &target.join(obj.file_name()))?;
        }
    }
    Ok(())
}

/// Copy `from` to `to` (a new file) while hashing it; `FG-GIT-0037
/// bundle_too_large` past `max` bytes. The sha256, lower-case hex.
async fn copy_hashed(from: &Path, to: &Path, max: u64) -> Result<String, ForgeError> {
    use sha2::{Digest as _, Sha256};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut src = tokio::fs::File::open(from)
        .await
        .map_err(|e| candidate_invalid("bundle_unreadable").with_source(e))?;
    let mut dst = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .await
        .map_err(|e| storage_io("autofix_bundle_copy", e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = src
            .read(&mut buf)
            .await
            .map_err(|e| candidate_invalid("bundle_unreadable").with_source(e))?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        if total > max {
            return Err(candidate_invalid("bundle_too_large").with_context("limit_bytes", max));
        }
        let chunk = buf.get(..n).unwrap_or_default();
        hasher.update(chunk);
        dst.write_all(chunk)
            .await
            .map_err(|e| storage_io("autofix_bundle_copy", e))?;
    }
    dst.flush()
        .await
        .map_err(|e| storage_io("autofix_bundle_copy", e))?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The first bytes of `bundle` (its header and a little more).
async fn read_head(bundle: &Path) -> Result<Vec<u8>, ForgeError> {
    use tokio::io::AsyncReadExt as _;
    let file = tokio::fs::File::open(bundle)
        .await
        .map_err(|e| candidate_invalid("bundle_unreadable").with_source(e))?;
    let mut head = Vec::with_capacity(4096);
    file.take(MAX_HEADER_BYTES as u64)
        .read_to_end(&mut head)
        .await
        .map_err(|e| candidate_invalid("bundle_unreadable").with_source(e))?;
    Ok(head)
}

#[cfg(test)]
#[path = "autofix_tests.rs"]
mod tests;
