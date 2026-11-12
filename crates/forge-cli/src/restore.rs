//! `forge restore …` (M3 O1, ADR-0019 Amendment plan 11 P1, runbook
//! backup-restore). An operator command (actor `system`, `actor_label =
//! forge-cli:<OS account>`), never reachable from HTTP; in production every
//! mutating step is an approved runbook step (CLAUDE.md §3.7).
//!
//! The drill, in order (forge serve and forge worker stopped):
//!
//! 1. `forge restore begin` — writes `<storage.root>/.forge-restore-pending`;
//!    while it exists `forge serve` and `forge worker` refuse to start
//!    (FG-SYS-0009 `restore_pending`): nothing is served before step 5.
//! 2. The database: `pgbackrest --stanza=<s> --set=<label> --delta restore`
//!    (PostgreSQL stopped; the label is in the generation's manifest) —
//!    a runbook step, not forge.
//! 3. `forge restore repositories --generation <g> [--replace]` — the
//!    encrypted archive back into `storage.root` (checksum first; each
//!    repository written beside its place and renamed in; `--replace` moves
//!    an existing one to `<storage.root>/pre-restore-<g>/`).
//! 4. `forge restore verify` — read-only: the database's expectations
//!    against the repositories (see [`verify`]).
//! 5. `forge restore finish --generation <g>` — verify again, then revoke
//!    every credential, re-apply the narrowings of the state copy (or remove
//!    every administrator flag without one), give the event feed a new epoch
//!    (every integration's cursor answers 410 `epoch_mismatch`), write the
//!    `restore.finished` audit row and remove the marker.
//!
//! `forge restore regrant-admin <username>` gives the flag back to a user
//! the restore listed (the runbook's check first).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_core::config::Config;
use forge_core::{RefName, RepoId, RequestId};
use forge_db::restore::{self as db_restore, state};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::maintenance::FsckMode;
use forge_git::{Git, StorageRoot};
use sha2::{Digest, Sha256};

use crate::backup::{self, archive, crypt, Prepared};

/// The marker of a restore in progress, in the storage root.
pub const MARKER: &str = ".forge-restore-pending";
/// One git step of the verification (fsck, for-each-ref).
const GIT_STEP_TIMEOUT: Duration = Duration::from_secs(600);
/// Findings listed in the report (all are counted).
const LIST_MAX: usize = 50;

fn failed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RestoreVerificationFailed).with_context("reason", reason)
}

fn io_failed(reason: &'static str, operation: &'static str, e: io::Error) -> ForgeError {
    failed(reason)
        .with_context("operation", operation)
        .with_source(e)
}

/// `forge serve` / `forge worker`: refuse to start while a restore is
/// pending (FG-SYS-0009 `restore_pending`).
pub fn refuse_if_pending(storage_root: &Path) -> Result<(), ForgeError> {
    let marker = storage_root.join(MARKER);
    match fs::symlink_metadata(&marker) {
        Ok(_) => Err(failed("restore_pending").with_context("path", marker.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_failed("restore_pending", "inspect_marker", e)
            .with_context("path", marker.display())),
    }
}

fn require_marker(storage_root: &Path) -> Result<(), ForgeError> {
    match fs::symlink_metadata(storage_root.join(MARKER)) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(failed("not_in_restore")),
        Err(e) => Err(io_failed("not_in_restore", "inspect_marker", e)),
    }
}

/// fsync one directory (Unix; elsewhere the rename is the only barrier).
fn sync_tree_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// fsync `dir` and every directory below it (the files were synced when
/// written).
fn sync_tree(dir: &Path) -> io::Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            sync_tree(&e.path())?;
        }
    }
    sync_tree_dir(dir)
}

/// The restore the marker names (`begin=<request id of forge restore
/// begin>`): every `restore.*` audit row of `finish` carries it, so a
/// re-run of `finish` knows its earlier run.
pub fn begin_id(storage_root: &Path) -> Result<String, ForgeError> {
    let text = fs::read_to_string(storage_root.join(MARKER))
        .map_err(|e| io_failed("not_in_restore", "read_marker", e))?;
    text.lines()
        .find_map(|l| l.strip_prefix("begin="))
        .map(str::trim)
        .filter(|b| !b.is_empty() && b.len() <= 128)
        .map(str::to_owned)
        .ok_or_else(|| failed("not_in_restore").with_context("detail", "marker_without_begin"))
}

/// Step 1: write the marker. Again while one exists: kept (the same
/// restore goes on).
pub fn begin(storage_root: &Path, request_id: &RequestId) -> Result<String, ForgeError> {
    let marker = storage_root.join(MARKER);
    if begin_id(storage_root).is_ok() {
        return Ok(format!(
            "a restore is already begun ({}); go on with its steps\n",
            marker.display()
        ));
    }
    // A marker without `begin=` (an older format) is replaced; a symlink in
    // its place is never followed (create-new refuses it once removed).
    if let Ok(meta) = fs::symlink_metadata(&marker) {
        if meta.file_type().is_symlink() || meta.is_file() {
            fs::remove_file(&marker).map_err(|e| io_failed("storage_io", "replace_marker", e))?;
        }
    }
    let mut f = backup::create_private_file(&marker)
        .map_err(|e| io_failed("storage_io", "write_marker", e))?;
    f.write_all(format!("begin={request_id}\n").as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| io_failed("storage_io", "write_marker", e))?;
    tracing::info!(
        target: "forge_cli",
        event = "forge.restore.begun",
        request_id = %request_id,
        "restore begun: forge serve and forge worker refuse to start until forge restore finish"
    );
    Ok(format!(
        "restore begun ({}): forge serve and forge worker refuse to start until `forge restore \
         finish`.\nNext: restore the database with pgBackRest (runbook backup-restore), then \
         `forge restore repositories --generation <g>`.\n",
        marker.display()
    ))
}

fn generation_dir(prepared: &Prepared, generation: &str) -> Result<PathBuf, ForgeError> {
    if !backup::is_generation_name(generation) {
        return Err(failed("generation_missing").with_context("generation", generation));
    }
    let dir = prepared.dir.join(generation);
    if !dir.join(backup::MANIFEST).is_file() {
        return Err(failed("generation_missing").with_context("generation", generation));
    }
    Ok(dir)
}

/// The repository an archive path belongs to: `repos/<shard>/<id>.git/…`
/// where `<shard>` is the one the storage derives from `<id>`.
fn repo_of(root: &StorageRoot, path: &str) -> Option<(RepoId, bool)> {
    let mut parts = path.splitn(4, '/');
    let (repos, shard, name) = (parts.next()?, parts.next()?, parts.next()?);
    if repos != "repos" {
        return None;
    }
    let id: i64 = name.strip_suffix(".git")?.parse().ok()?;
    let repo = RepoId::new(id);
    let expected = root.path_for(repo);
    let ok = expected
        .parent()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        == Some(shard);
    ok.then_some((repo, parts.next().is_none()))
}

/// Step 3. The manifest's MAC and the archive's checksum are checked before
/// anything is written; every directory is created 0700 and every file
/// 0600; the whole step stops at `timeout` (`backup.repo_timeout_secs`,
/// FG-SYS-0009 `restore_timeout`).
pub fn repositories(
    prepared: &Prepared,
    storage: &StorageRoot,
    generation: &str,
    replace: bool,
    timeout: Duration,
    request_id: &RequestId,
) -> Result<String, ForgeError> {
    require_marker(storage.path())?;
    let deadline = std::time::Instant::now() + timeout;
    let late = || failed("restore_timeout").with_context("timeout_secs", timeout.as_secs());
    let dir = generation_dir(prepared, generation)?;
    let manifest = backup::read_manifest_verified(&dir, &prepared.key)
        .map_err(|e| e.with_context("generation", generation))?;
    let archive_path = dir.join(&manifest.archive.file);
    let corrupt = |detail: &'static str| {
        failed("archive_corrupt")
            .with_context("generation", generation)
            .with_context("detail", detail)
    };
    // The checksum first: nothing is written from a changed file.
    let mut hasher = Sha256::new();
    let mut f =
        fs::File::open(&archive_path).map_err(|e| io_failed("archive_corrupt", "open", e))?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if std::time::Instant::now() > deadline {
            return Err(late());
        }
        let n = io::Read::read(&mut f, &mut buf)
            .map_err(|e| io_failed("archive_corrupt", "read", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    if backup::hex(&hasher.finalize()) != manifest.archive.sha256 {
        return Err(corrupt("checksum"));
    }
    let listed: BTreeSet<i64> = manifest.repositories.iter().map(|r| r.id).collect();
    // One place per run: a second `--replace` never meets the first one's.
    let pre = storage
        .path()
        .join(format!("pre-restore-{generation}-{request_id}"));
    for id in &listed {
        let target = storage.path_for(RepoId::new(*id));
        if target.exists() && !replace {
            return Err(failed("target_exists").with_context("repo_id", id));
        }
    }
    let file =
        fs::File::open(&archive_path).map_err(|e| io_failed("archive_corrupt", "open", e))?;
    let opened = crypt::OpenReader::new(
        BufReader::new(file),
        &prepared.key,
        crypt::Kind::Repositories,
        generation.as_bytes(),
    )
    .map_err(|_| corrupt("decrypt"))?;
    let mut reader = archive::ArchiveReader::new(BufReader::new(opened));
    let mut current: Option<(RepoId, PathBuf)> = None;
    let mut restored = Vec::new();
    let finish_repo = |cur: Option<(RepoId, PathBuf)>,
                       restored: &mut Vec<i64>|
     -> Result<(), ForgeError> {
        let Some((repo, tmp)) = cur else {
            return Ok(());
        };
        let target = storage.path_for(repo);
        // No long-lived git of this process holds the old copy open, and
        // nothing cached of it survives (forge serve and the worker are
        // stopped for a restore: theirs are gone already).
        forge_git::browse::release_repository(storage, repo);
        // Every directory of the restored repository is on disk (its entries
        // were synced file by file) before the rename makes it visible.
        sync_tree(&tmp).map_err(|e| io_failed("storage_io", "sync_dir", e))?;
        if target.exists() {
            backup::create_private_dir(&pre)
                .map_err(|e| io_failed("storage_io", "create_pre_restore", e))?;
            let aside = pre.join(format!("{}.git", repo.get()));
            fs::rename(&target, &aside).map_err(|e| io_failed("storage_io", "move_aside", e))?;
        }
        fs::rename(&tmp, &target).map_err(|e| io_failed("storage_io", "rename_into_place", e))?;
        if let Some(parent) = target.parent() {
            sync_tree_dir(parent).map_err(|e| io_failed("storage_io", "sync_dir", e))?;
        }
        restored.push(repo.get());
        Ok(())
    };
    let result: Result<(), ForgeError> = (|| {
        loop {
            if std::time::Instant::now() > deadline {
                return Err(late());
            }
            let entry = reader.next_entry().map_err(|_| corrupt("entry"))?;
            let Some(entry) = entry else {
                break;
            };
            let path = match &entry {
                archive::Entry::Dir(p) | archive::Entry::File(p, _) => p.clone(), // clone: the entry is matched again below
            };
            let (repo, is_root) = repo_of(storage, &path).ok_or_else(|| corrupt("path"))?;
            if !listed.contains(&repo.get()) {
                return Err(corrupt("unlisted_repository"));
            }
            if is_root {
                if !matches!(entry, archive::Entry::Dir(_)) {
                    return Err(corrupt("root_not_a_directory"));
                }
                finish_repo(current.take(), &mut restored)?;
                let target = storage.path_for(repo);
                let parent = target.parent().ok_or_else(|| corrupt("path"))?;
                fs::create_dir_all(parent)
                    .map_err(|e| io_failed("storage_io", "create_shard", e))?;
                let tmp = parent.join(format!(".restore-{}.git", repo.get()));
                if tmp.exists() {
                    fs::remove_dir_all(&tmp)
                        .map_err(|e| io_failed("storage_io", "remove_tmp", e))?;
                }
                backup::create_private_dir(&tmp)
                    .map_err(|e| io_failed("storage_io", "create_tmp", e))?;
                current = Some((repo, tmp));
                continue;
            }
            let Some((cur, tmp)) = &current else {
                return Err(corrupt("entry_before_root"));
            };
            if *cur != repo {
                return Err(corrupt("interleaved_repositories"));
            }
            let rel = archive::checked(&path).map_err(|_| corrupt("path"))?;
            let root_rel = archive::checked(
                &archive::relative(storage.path(), &storage.path_for(repo))
                    .map_err(|_| corrupt("path"))?,
            )
            .map_err(|_| corrupt("path"))?;
            let inner = rel.strip_prefix(&root_rel).map_err(|_| corrupt("path"))?;
            let out = tmp.join(inner);
            match entry {
                archive::Entry::Dir(_) => {
                    backup::create_private_dir(&out)
                        .map_err(|e| io_failed("storage_io", "create_dir", e))?;
                }
                archive::Entry::File(_, size) => {
                    if let Some(parent) = out.parent() {
                        backup::create_private_dir(parent)
                            .map_err(|e| io_failed("storage_io", "create_dir", e))?;
                    }
                    let mut f = backup::create_private_file(&out)
                        .map_err(|e| io_failed("storage_io", "create_file", e))?;
                    reader
                        .content(size, &mut f)
                        .map_err(|_| corrupt("content"))?;
                    // On disk before the repository is renamed into place.
                    f.sync_all()
                        .map_err(|e| io_failed("storage_io", "sync_file", e))?;
                }
            }
        }
        finish_repo(current.take(), &mut restored)
    })();
    if let Err(e) = result {
        if let Some((_, tmp)) = current {
            if let Err(rm) = fs::remove_dir_all(&tmp) {
                tracing::warn!(
                    target: "forge_cli",
                    event = "forge.restore.tmp_not_removed",
                    path = %tmp.display(),
                    error = %rm,
                    "a partly written repository was not removed"
                );
            }
        }
        return Err(e);
    }
    tracing::info!(
        target: "forge_cli",
        event = "forge.restore.repositories_restored",
        request_id = %request_id,
        generation,
        repositories = restored.len(),
        replaced = replace,
        "repositories restored"
    );
    Ok(format!(
        "restored {} repositories of generation {generation} into {}\nNext: `forge restore \
         verify` (the commit-graph and multi-pack-index files are not in the backup: the next \
         maintenance writes them).\n",
        restored.len(),
        storage.path().display()
    ))
}

/// What `verify` can find (FG-SYS-0009 cause).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    RepositoryMissing,
    RepositoryCorrupt,
    VersionRefMissing,
    VersionRefMismatch,
    PurgedRefPresent,
    VersionRefUnknown,
    RepositoryUnknown,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RepositoryMissing => "repository_missing",
            Self::RepositoryCorrupt => "repository_corrupt",
            Self::VersionRefMissing => "version_ref_missing",
            Self::VersionRefMismatch => "version_ref_mismatch",
            Self::PurgedRefPresent => "purged_ref_present",
            Self::VersionRefUnknown => "version_ref_unknown",
            Self::RepositoryUnknown => "repository_unknown",
        }
    }

    /// The database is older than the repositories (a database-only
    /// restore): `finish --accept-db-behind` may go on.
    pub fn db_behind(self) -> bool {
        matches!(self, Self::VersionRefUnknown | Self::RepositoryUnknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: Kind,
    pub repo: i64,
    pub detail: String,
}

/// The result of [`verify`].
#[derive(Debug, Clone, Default)]
pub struct Verified {
    pub repositories: usize,
    pub versions: usize,
    pub findings: Vec<Finding>,
}

impl Verified {
    pub fn counts(&self) -> BTreeMap<Kind, usize> {
        let mut out = BTreeMap::new();
        for f in &self.findings {
            *out.entry(f.kind).or_insert(0) += 1;
        }
        out
    }

    /// FG-SYS-0009 for the findings (`None`: clean, or only database-behind
    /// findings when `accept_db_behind`).
    pub fn error(&self, accept_db_behind: bool) -> Option<ForgeError> {
        let blocking: Vec<&Finding> = self
            .findings
            .iter()
            .filter(|f| !(accept_db_behind && f.kind.db_behind()))
            .collect();
        let first = blocking.first()?;
        let counts = self.counts();
        let kinds: Vec<&str> = counts.keys().map(|k| k.as_str()).collect();
        let mut e = ForgeError::new(ErrorCode::RestoreVerificationFailed)
            .with_context("findings", kinds.join(","))
            .with_context(
                "first",
                format!(
                    "{} repo_id={} {}",
                    first.kind.as_str(),
                    first.repo,
                    first.detail
                ),
            );
        for (k, n) in counts {
            e = e.with_context(k.as_str(), n);
        }
        Some(e)
    }

    pub fn report(&self) -> String {
        let mut out = format!(
            "verified {} repositories and {} merge request versions: {}\n",
            self.repositories,
            self.versions,
            if self.findings.is_empty() {
                "consistent".to_owned()
            } else {
                format!("{} finding(s)", self.findings.len())
            }
        );
        for (k, n) in self.counts() {
            let behind = if k.db_behind() {
                " (the database is older than the repositories)"
            } else {
                ""
            };
            out.push_str(&format!("  {}: {n}{behind}\n", k.as_str()));
        }
        for f in self.findings.iter().take(LIST_MAX) {
            out.push_str(&format!(
                "  - {} repo_id={} {}\n",
                f.kind.as_str(),
                f.repo,
                f.detail
            ));
        }
        if self.findings.len() > LIST_MAX {
            out.push_str(&format!("  … {} more\n", self.findings.len() - LIST_MAX));
        }
        out
    }
}

/// The repository ids that have a directory under `<root>/repos/`.
fn repositories_on_disk(root: &StorageRoot) -> Result<BTreeSet<i64>, ForgeError> {
    let repos = root.path().join("repos");
    let mut out = BTreeSet::new();
    let shards = match fs::read_dir(&repos) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(io_failed("storage_io", "list_repositories", e)),
    };
    for shard in shards {
        let shard = shard.map_err(|e| io_failed("storage_io", "list_repositories", e))?;
        if !shard.path().is_dir() {
            continue;
        }
        let entries = fs::read_dir(shard.path())
            .map_err(|e| io_failed("storage_io", "list_repositories", e))?;
        for e in entries {
            let e = e.map_err(|e| io_failed("storage_io", "list_repositories", e))?;
            let name = e.file_name();
            let id = name
                .to_str()
                .and_then(|n| n.strip_suffix(".git"))
                .and_then(|n| n.parse::<i64>().ok());
            if let Some(id) = id {
                if root.path_for(RepoId::new(id)) == e.path() {
                    out.insert(id);
                }
            }
        }
    }
    Ok(out)
}

/// The refs under `refs/merge-requests/<iid>/` that `verify` does not
/// compare for a known merge request (forge-git `hidden_refs`): derived
/// from the versions or recomputed, so the database holds no expectation.
const DERIVED_MR_REFS: [&str; 2] = ["head", "merge"];

/// Step 4: compare the database's expectations with the repositories.
///
/// - every ready repository row has its directory (`repository_missing`)
///   and passes `git fsck --connectivity-only` (`repository_corrupt`);
/// - every `merge_request_versions` row's ref
///   `refs/merge-requests/<iid>/v<n>` holds its `head_sha`
///   (`version_ref_missing` / `version_ref_mismatch`), or is gone when its
///   refs were purged (`purged_ref_present`);
/// - the database-behind findings: a version ref without its row
///   (`version_ref_unknown`; `head` and `merge` of a known merge request
///   are derived, never a finding), a repository directory without its row
///   (`repository_unknown`).
///
/// Read-only. A git or database failure other than a finding is returned.
pub async fn verify(db: &Db, storage: &StorageRoot, git: &Git) -> Result<Verified, ForgeError> {
    let rows = db_restore::repositories(db).await?;
    let versions = db_restore::expected_versions(db).await?;
    let iids = db_restore::merge_request_iids(db).await?;
    let mut out = Verified {
        repositories: rows.iter().filter(|r| r.ready).count(),
        versions: versions.len(),
        findings: Vec::new(),
    };
    let known: BTreeSet<i64> = rows.iter().map(|r| r.repo.get()).collect();
    for id in repositories_on_disk(storage)? {
        if !known.contains(&id) {
            out.findings.push(Finding {
                kind: Kind::RepositoryUnknown,
                repo: id,
                detail: String::new(),
            });
        }
    }
    let mut by_repo: BTreeMap<i64, Vec<&db_restore::ExpectedVersion>> = BTreeMap::new();
    for v in &versions {
        by_repo.entry(v.repo.get()).or_default().push(v);
    }
    let pattern = RefName::parse("refs/merge-requests")?;
    for row in rows.iter().filter(|r| r.ready) {
        let id = row.repo.get();
        match storage.existing(row.repo) {
            Ok(_) => {}
            Err(e) if e.code() == ErrorCode::StorageMissing => {
                out.findings.push(Finding {
                    kind: Kind::RepositoryMissing,
                    repo: id,
                    detail: String::new(),
                });
                continue;
            }
            Err(e) => return Err(e),
        }
        match git
            .fsck(storage, row.repo, FsckMode::Connectivity, GIT_STEP_TIMEOUT)
            .await
        {
            Ok(()) => {}
            Err(e) if e.code() == ErrorCode::RepositoryCorrupt => {
                forge_core::telemetry::log_failure(&e, &RequestId::generate());
                out.findings.push(Finding {
                    kind: Kind::RepositoryCorrupt,
                    repo: id,
                    detail: String::new(),
                });
                continue;
            }
            Err(e) => return Err(e),
        }
        let refs = git
            .read_refs(storage, row.repo, &pattern, GIT_STEP_TIMEOUT)
            .await?;
        let mut expected: BTreeSet<String> = BTreeSet::new();
        for v in by_repo.get(&id).map(Vec::as_slice).unwrap_or_default() {
            let name = v.ref_name();
            let actual = RefName::parse(&name)
                .ok()
                .and_then(|n| refs.get(&n))
                .map(|o| o.as_str().to_owned());
            match (v.purged, actual) {
                (false, None) => out.findings.push(Finding {
                    kind: Kind::VersionRefMissing,
                    repo: id,
                    detail: format!("{name} expected {}", v.head_sha),
                }),
                (false, Some(a)) if a != v.head_sha => out.findings.push(Finding {
                    kind: Kind::VersionRefMismatch,
                    repo: id,
                    detail: format!("{name} expected {} actual {a}", v.head_sha),
                }),
                (true, Some(_)) => out.findings.push(Finding {
                    kind: Kind::PurgedRefPresent,
                    repo: id,
                    detail: name.clone(), // clone: also kept in `expected`
                }),
                (false, Some(_)) | (true, None) => {}
            }
            expected.insert(name);
        }
        for name in refs.keys() {
            let n = name.as_str();
            if expected.contains(n) {
                continue;
            }
            // The derived refs of a known merge request have no expectation
            // in the database: `head` is the newest version's, `merge` the
            // mergeability check's merge result (M4 C7, recomputed on every
            // check and pruned once the merge request is closed or merged).
            let iid = n
                .strip_prefix("refs/merge-requests/")
                .and_then(|rest| rest.split_once('/'))
                .and_then(|(iid, tail)| iid.parse::<i64>().ok().map(|i| (i, tail)));
            if let Some((iid, tail)) = iid {
                if DERIVED_MR_REFS.contains(&tail) && iids.contains(&(id, iid)) {
                    continue;
                }
            }
            out.findings.push(Finding {
                kind: Kind::VersionRefUnknown,
                repo: id,
                detail: n.to_owned(),
            });
        }
    }
    out.findings.sort_by_key(|f| (f.kind, f.repo));
    Ok(out)
}

/// `forge restore verify` as a command: the report, or FG-SYS-0009.
pub async fn verify_command(
    db: &Db,
    storage: &StorageRoot,
    git: &Git,
    request_id: &RequestId,
) -> Result<String, ForgeError> {
    let v = verify(db, storage, git).await?;
    let report = v.report();
    match v.error(false) {
        None => {
            tracing::info!(
                target: "forge_cli",
                event = "forge.restore.verified",
                request_id = %request_id,
                repositories = v.repositories,
                versions = v.versions,
                "restore verified: consistent"
            );
            Ok(report)
        }
        Some(e) => {
            // The full list goes to stdout; the error carries the counts.
            let mut out = io::stdout().lock();
            if let Err(w) = out.write_all(report.as_bytes()) {
                tracing::warn!(target: "forge_cli", error = %w, "the verify report was not printed");
            }
            Err(e)
        }
    }
}

/// What `finish` did.
#[derive(Debug, Clone)]
pub struct Finished {
    pub verified: Verified,
    pub revoked: db_restore::Revoked,
    pub reapplied: Option<(PathBuf, state::Reapplied)>,
    pub unusable: Option<crate::state_copy::Unusable>,
    /// The users whose flag this restore removed (this run or an earlier run
    /// of the same restore: read from its `restore.admin_flags_removed` row).
    pub admins_removed: Vec<db_restore::AdminRemoved>,
    /// An earlier `finish` of the same restore (`begin`) wrote rows: this
    /// one went on where it stopped (every step is per unit and re-runnable).
    pub rerun: bool,
    pub begin: String,
    /// Active human administrators left (0: `forge admin bootstrap-admin`).
    pub admins_left: i64,
}

/// The choices of `forge restore finish`.
pub struct FinishArgs<'a> {
    pub generation: &'a str,
    pub accept_db_behind: bool,
    /// Use an older readable state copy when the newest one does not open
    /// (otherwise that is treated as no copy: every administrator flag is
    /// removed).
    pub allow_older_copy: bool,
}

/// Step 5 without the epoch (which needs the maintenance connection: the
/// caller does it before removing the marker).
pub async fn finish_with(
    db: &Db,
    prepared: &Prepared,
    storage: &StorageRoot,
    git: &Git,
    args: &FinishArgs<'_>,
    request_id: &RequestId,
) -> Result<Finished, ForgeError> {
    require_marker(storage.path())?;
    let begin = begin_id(storage.path())?;
    let generation = args.generation;
    let dir = generation_dir(prepared, generation)?;
    let manifest = backup::read_manifest_verified(&dir, &prepared.key)
        .map_err(|e| e.with_context("generation", generation))?;
    let verified = verify(db, storage, git).await?;
    if let Some(e) = verified.error(args.accept_db_behind) {
        return Err(e);
    }
    let (rerun, _) = db_restore::earlier_run(db, &begin).await?;
    let ctx = crate::admin_token::context(request_id);
    let revoked = db_restore::revoke_all_credentials(db, &ctx, &begin).await?;
    let (reapplied, unusable) = match crate::state_copy::newest_usable(
        prepared.state_copy_dir.as_deref(),
        &prepared.key,
        manifest.db.completed_at_micros,
        args.allow_older_copy,
    ) {
        Ok((path, copy)) => {
            let done = state::reapply(db, &ctx, &copy).await?;
            (Some((path, done)), None)
        }
        Err(why) => {
            let removed =
                db_restore::remove_all_admin_flags(db, &ctx, why.as_str(), &begin).await?;
            tracing::warn!(
                target: "forge_cli",
                event = "forge.restore.admin_flags_removed",
                request_id = %request_id,
                reason = why.as_str(),
                users = removed.len(),
                "no usable state copy: every administrator flag was removed"
            );
            (None, Some(why))
        }
    };
    // The list of the whole restore (a re-run removes nothing new).
    let (_, admins_removed) = db_restore::earlier_run(db, &begin).await?;
    let admins_left = db_restore::active_admins(db).await?;
    if admins_left == 0 {
        tracing::warn!(
            target: "forge_cli",
            event = "forge.restore.no_admin_left",
            request_id = %request_id,
            "no active administrator is left after the restore: regrant-admin or bootstrap-admin"
        );
    }
    Ok(Finished {
        verified,
        revoked,
        reapplied,
        unusable,
        admins_removed,
        rerun,
        begin,
        admins_left,
    })
}

fn finished_details(f: &Finished, generation: &str, epoch: Option<&str>) -> serde_json::Value {
    let r = f.reapplied.as_ref().map(|(_, r)| *r).unwrap_or_default();
    serde_json::json!({
        "begin": f.begin,
        "rerun": f.rerun,
        "generation": generation,
        "repositories": f.verified.repositories,
        "db_behind_findings": f.verified.findings.iter().filter(|x| x.kind.db_behind()).count(),
        "revoked_tokens": f.revoked.tokens,
        "revoked_sessions": f.revoked.sessions,
        "disabled_webhooks": f.revoked.webhooks,
        "disabled_push_mirrors": f.revoked.push_mirrors,
        "disabled_integrations": f.revoked.integrations,
        "rejected_assertions": f.revoked.assertions,
        "state_copy": f.reapplied.is_some(),
        "state_copy_unusable": f.unusable.as_ref().map(|u| u.as_str()),
        "reapplied": r.total(),
        "admin_flags_removed": f.admins_removed.len(),
        "admins_left": f.admins_left,
        "new_epoch": epoch,
    })
}

fn finish_report(f: &Finished, epoch: &str) -> String {
    let mut out = f.verified.report();
    if f.rerun {
        out.push_str(
            "re-run of this restore's finish: the steps an earlier run did changed nothing again\n",
        );
    }
    out.push_str(&format!(
        "revoked: {} token(s) and {} session(s) of {} user(s); disabled {} webhook(s) and {} \
         push mirror(s) of {} project(s), {} integration(s); rejected {} act-as assertion(s)\n",
        f.revoked.tokens,
        f.revoked.sessions,
        f.revoked.users,
        f.revoked.webhooks,
        f.revoked.push_mirrors,
        f.revoked.projects,
        f.revoked.integrations,
        f.revoked.assertions
    ));
    match (&f.reapplied, &f.unusable) {
        (Some((path, r)), _) => out.push_str(&format!(
            "state copy {}: re-applied {} narrowing(s) (users disabled {}, admin flags removed \
             {}, members removed {}, roles lowered {}, protected refs created {}, push roles \
             raised {}, merge rules tightened {}, approval rules tightened {}, approvers \
             narrowed {}, merge settings tightened {}, projects made private {}, check grants \
             revoked {})\n",
            path.display(),
            r.total(),
            r.users_disabled,
            r.admin_flags_removed,
            r.members_removed,
            r.roles_lowered,
            r.protected_refs_created,
            r.push_roles_raised,
            r.merge_rules_tightened,
            r.approval_rules_tightened,
            r.approvers_narrowed,
            r.merge_settings_tightened,
            r.visibility_narrowed,
            r.check_grants_revoked
        )),
        (None, why) => {
            out.push_str(&format!(
                "no usable state copy ({}): every administrator flag was removed — check each \
                 person, then `forge restore regrant-admin <username>`:\n",
                why.as_ref().map_or("unknown", |w| w.as_str())
            ));
            for a in &f.admins_removed {
                out.push_str(&format!("  - {} (user {})\n", a.username, a.user.get()));
            }
        }
    }
    if f.admins_left == 0 {
        out.push_str(
            "no active administrator is left: `forge restore regrant-admin <username>` for a \
             listed user, or `forge admin bootstrap-admin --user <username>`\n",
        );
    }
    out.push_str(&format!(
        "event feed epoch: {epoch} (every integration resyncs: 410 epoch_mismatch)\nrestore \
         finished: forge serve and forge worker may start. Re-issue the credentials and enable \
         the push mirrors and integrations again (runbook backup-restore, \"Re-issue\").\n"
    ));
    out
}

/// `forge restore finish`.
pub fn finish(
    cfg: &Config,
    args: &FinishArgs<'_>,
    request_id: &RequestId,
) -> Result<String, ForgeError> {
    let prepared = backup::prepare(&cfg.backup)?;
    let storage = crate::worker::provision::Storage::open(cfg.storage_root()?)?;
    crate::runtime::block_on(async {
        let db = Db::connect(&cfg.database).await?;
        let done = finish_all(
            &db,
            &cfg.database,
            &prepared,
            &storage.root,
            &storage.git,
            args,
            request_id,
        )
        .await;
        db.close().await;
        done
    })
}

/// Step 5 whole: [`finish_with`], the new epoch (`database.maint_url`; the
/// `feed.epoch_rotated` audit row and FG-DB-0014), the `restore.finished`
/// row, then the marker removed. Per unit and re-runnable: a failure leaves
/// the marker, and running it again goes on where it stopped (`rerun`).
pub async fn finish_all(
    db: &Db,
    database: &forge_core::config::DatabaseConfig,
    prepared: &Prepared,
    storage: &StorageRoot,
    git: &Git,
    args: &FinishArgs<'_>,
    request_id: &RequestId,
) -> Result<String, ForgeError> {
    let generation = args.generation;
    let done = finish_with(db, prepared, storage, git, args, request_id).await?;
    let mut maint = Db::connect_maint(database)
        .await
        .map_err(|e| e.with_context("step", "new_epoch"))?;
    let rotated = forge_db::feed::new_epoch(&mut maint, request_id).await;
    maint.close().await;
    let rotated = rotated.map_err(|e| e.with_context("step", "new_epoch"))?;
    forge_core::telemetry::log_failure(&forge_db::feed::epoch_changed(&rotated), request_id);
    let epoch = backup::hex(&rotated.new);
    db_restore::record_finished(
        db,
        &crate::admin_token::context(request_id),
        finished_details(&done, generation, Some(&epoch)),
    )
    .await?;
    fs::remove_file(storage.path().join(MARKER))
        .map_err(|e| io_failed("storage_io", "remove_marker", e))?;
    tracing::info!(
        target: "forge_cli",
        event = "forge.restore.finished",
        request_id = %request_id,
        generation,
        revoked_tokens = done.revoked.tokens,
        revoked_sessions = done.revoked.sessions,
        disabled_webhooks = done.revoked.webhooks,
        state_copy = done.reapplied.is_some(),
        admin_flags_removed = done.admins_removed.len(),
        "restore finished"
    );
    Ok(finish_report(&done, &epoch))
}

/// `forge restore regrant-admin`.
pub async fn regrant_admin_with(
    db: &Db,
    username: &str,
    request_id: &RequestId,
) -> Result<String, ForgeError> {
    let ctx = crate::admin_token::context(request_id);
    match db_restore::regrant_admin(db, &ctx, username).await? {
        db_restore::Regranted::Granted(user) => {
            tracing::info!(
                target: "forge_cli",
                event = "forge.restore.admin_regranted",
                request_id = %request_id,
                user_id = user.get(),
                "administrator flag given back"
            );
            Ok(format!("user {username}: administrator again\n"))
        }
        db_restore::Regranted::AlreadyAdmin(_) => Ok(format!(
            "user {username}: already an administrator (nothing changed)\n"
        )),
    }
}

#[cfg(test)]
#[path = "restore_tests.rs"]
mod tests;
