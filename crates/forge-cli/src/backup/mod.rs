//! `forge backup` (M3 O1, ADR-0019 Amendment plan 11 P1, plan 11 §10 Q11,
//! runbook backup-restore): one **generation** in `backup.dir`, the
//! database first, then the repositories.
//!
//! ```text
//! <backup.dir>/
//!   .lock                    one running backup (create-new; a stale one is a runbook step)
//!   <YYYYMMDDTHHMMSSZ>/      a complete generation (the database clock, UTC)
//!     manifest.json          what it holds (no secret; mode 0600)
//!     repositories.fgbk      every ready repository, AES-256-GCM (crypt.rs, archive.rs)
//!   <name>.partial/          a run that did not finish (removed by the next run)
//! ```
//!
//! 1. Refuse an unsafe place (FG-SYS-0008 `insecure_permissions`): the
//!    backup directory (and the state copy directory) must be a real
//!    directory owned by the account that runs forge with no access for
//!    group or others (Unix `0700`); the key file `0600` (forge-core);
//!    neither the key nor the state copy inside `backup.dir`.
//! 2. The database ([`db_tool`], pgBackRest): its label and the database
//!    clock after it go into the manifest. The repositories are copied
//!    **after** it, so every ref the database expects exists in the copy
//!    (pushes in between only add; `forge restore verify` reports them).
//! 3. Each ready repository is copied refs-before-objects into a staging
//!    directory, checked with `git fsck --connectivity-only` (copied again
//!    up to [`COPY_ATTEMPTS`] times when a gc replaced packs meanwhile),
//!    appended to the archive and removed from the staging directory.
//! 4. The manifest is written last, the directory renamed from `.partial`,
//!    and the oldest generations beyond `backup.generations` removed.
//! 5. A state copy is written (when configured): one copy at least as new
//!    as every backup's database step.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use forge_core::config::{BackupConfig, BackupKey, Config};
use forge_core::RequestId;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::maintenance::FsckMode;
use forge_git::{Git, StorageRoot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod archive;
pub mod crypt;
pub mod db_tool;

use db_tool::{failed, DbBackupTool, PgBackRest};

/// Copies of one repository before the backup gives up on it.
pub const COPY_ATTEMPTS: u32 = 3;
pub const MANIFEST: &str = "manifest.json";
pub const ARCHIVE: &str = "repositories.fgbk";
const LOCK: &str = ".lock";
const PARTIAL: &str = ".partial";
const MANIFEST_FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestDb {
    pub tool: String,
    pub label: String,
    /// The database clock right after the database step.
    pub completed_at: String,
    pub completed_at_micros: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRepo {
    pub id: i64,
    pub project_id: i64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestArchive {
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
}

/// `manifest.json` of a generation: dates, the database backup's label,
/// the repositories and the archive's checksum. No secret, no path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub generation: String,
    pub request_id: String,
    pub db: ManifestDb,
    pub repositories: Vec<ManifestRepo>,
    pub archive: ManifestArchive,
    pub completed_at: String,
}

fn io_failed(reason: &'static str, operation: &'static str, e: io::Error) -> ForgeError {
    failed(reason)
        .with_context("operation", operation)
        .with_source(e)
}

/// A directory forge alone may enter: a real directory (no symlink),
/// owned by the effective user, without access for group or others.
/// Refused: FG-SYS-0008 `insecure_permissions` with `path` and `mode`.
pub fn check_private_dir(path: &Path) -> Result<PathBuf, ForgeError> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| io_failed("storage_io", "inspect", e).with_context("path", path.display()))?;
    let refuse = |why: &'static str| {
        failed("insecure_permissions")
            .with_context("path", path.display())
            .with_context("detail", why)
    };
    if meta.file_type().is_symlink() {
        return Err(refuse("symlink"));
    }
    if !meta.is_dir() {
        return Err(refuse("not_a_directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(refuse("group_or_other_access").with_context("mode", format!("{mode:o}")));
        }
        if meta.uid() != rustix::process::geteuid().as_raw() {
            return Err(refuse("not_owned_by_forge").with_context("mode", format!("{mode:o}")));
        }
    }
    fs::canonicalize(path).map_err(|e| {
        io_failed("storage_io", "canonicalize", e).with_context("path", path.display())
    })
}

/// The checked places and the key of a backup or a restore.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub dir: PathBuf,
    pub key: BackupKey,
    pub state_copy_dir: Option<PathBuf>,
}

/// Step 1 (module docs).
pub fn prepare(cfg: &BackupConfig) -> Result<Prepared, ForgeError> {
    let (Some(dir), Some(_)) = (&cfg.dir, &cfg.key_file) else {
        return Err(failed("not_configured"));
    };
    let dir = check_private_dir(dir)?;
    let key = cfg.read_key().ok_or_else(|| failed("not_configured"))??;
    if let Some(key_file) = &cfg.key_file {
        let key_file = fs::canonicalize(key_file).map_err(|e| {
            io_failed("storage_io", "canonicalize", e).with_context("path", key_file.display())
        })?;
        if key_file.starts_with(&dir) {
            return Err(failed("key_inside_backup").with_context("path", key_file.display()));
        }
    }
    let state_copy_dir = match &cfg.state_copy_dir {
        Some(s) => {
            let s = check_private_dir(s)?;
            if s.starts_with(&dir) || dir.starts_with(&s) {
                return Err(failed("key_inside_backup").with_context("path", s.display()));
            }
            Some(s)
        }
        None => None,
    };
    Ok(Prepared {
        dir,
        key,
        state_copy_dir,
    })
}

/// Create a file only its owner may read (0600), refusing an existing one.
pub fn create_private_file(path: &Path) -> io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600);
    }
    o.open(path)
}

/// Create a directory only its owner may enter (0700).
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        b.mode(0o700);
    }
    b.create(path)
}

fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// A generation's name: the database clock `YYYYMMDDTHHMMSSZ`.
pub fn is_generation_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 16
        && b[8] == b'T'
        && b[15] == b'Z'
        && b[..8].iter().chain(&b[9..15]).all(u8::is_ascii_digit)
}

/// The complete generations in `dir`, oldest first.
pub fn generations(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir)? {
        let e = e?;
        let Some(name) = e.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if is_generation_name(&name) && e.path().join(MANIFEST).is_file() {
            out.push((name, e.path()));
        }
    }
    out.sort();
    Ok(out)
}

/// Read a generation's manifest.
pub fn read_manifest(generation: &Path) -> Result<Manifest, ForgeError> {
    let text = fs::read(generation.join(MANIFEST)).map_err(|e| {
        io_failed("storage_io", "read_manifest", e).with_context("path", generation.display())
    })?;
    let m: Manifest = serde_json::from_slice(&text).map_err(|_| {
        failed("storage_io")
            .with_context("operation", "parse_manifest")
            .with_context("path", generation.display())
    })?;
    if m.format != MANIFEST_FORMAT {
        return Err(failed("storage_io")
            .with_context("operation", "manifest_format")
            .with_context("path", generation.display()));
    }
    Ok(m)
}

/// A writer that hashes and counts what passes.
struct Hashing<W: Write> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(data)?;
        self.hasher.update(&data[..n]);
        self.bytes = self.bytes.saturating_add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            s.push_str(&format!("{b:02x}"));
            s
        })
}

/// Removes the lock when the run ends (any way).
struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.0) {
            tracing::warn!(
                target: "forge_cli",
                error_code = %ErrorCode::BackupFailed,
                reason = "storage_io",
                operation = "remove_lock",
                error = %e,
                path = %self.0.display(),
                "forge.backup.lock_not_removed"
            );
        }
    }
}

fn take_lock(dir: &Path, request_id: &RequestId) -> Result<LockGuard, ForgeError> {
    let path = dir.join(LOCK);
    match create_private_file(&path) {
        Ok(mut f) => {
            f.write_all(format!("{request_id}\n").as_bytes())
                .map_err(|e| io_failed("storage_io", "write_lock", e))?;
            Ok(LockGuard(path))
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            Err(failed("backup_running").with_context("path", path.display()))
        }
        Err(e) => Err(io_failed("storage_io", "create_lock", e)),
    }
}

/// What a backup wrote.
#[derive(Debug, Clone)]
pub struct Done {
    pub manifest: Manifest,
    pub path: PathBuf,
    pub removed: Vec<String>,
    pub state_copy: Option<PathBuf>,
}

/// The parts of a backup run.
pub struct Run<'a, T: DbBackupTool> {
    pub db: &'a Db,
    pub cfg: &'a BackupConfig,
    pub prepared: &'a Prepared,
    pub storage: &'a StorageRoot,
    pub git: &'a Git,
    pub tool: &'a T,
    pub request_id: &'a RequestId,
}

impl<T: DbBackupTool> Run<'_, T> {
    /// Steps 2–5 (module docs).
    pub async fn run(&self) -> Result<Done, ForgeError> {
        let dir = &self.prepared.dir;
        let _lock = take_lock(dir, self.request_id)?;
        remove_partials(dir)?;
        let clock = forge_db::restore::clock(self.db).await?;
        let generation = clock.generation.clone(); // clone: kept in the manifest and the log
        let final_path = dir.join(&generation);
        if final_path.exists() {
            return Err(failed("storage_io")
                .with_context("operation", "generation_exists")
                .with_context("generation", &generation));
        }
        let partial = dir.join(format!("{generation}{PARTIAL}"));
        create_private_dir(&partial)
            .map_err(|e| io_failed("storage_io", "create_generation", e))?;
        let result = self.fill(&generation, &partial, &clock.rfc3339).await;
        let manifest = match result {
            Ok(m) => m,
            Err(e) => return Err(e.with_context("generation", &generation)),
        };
        fs::rename(&partial, &final_path)
            .map_err(|e| io_failed("storage_io", "rename_generation", e))?;
        sync_dir(dir).map_err(|e| io_failed("storage_io", "sync_dir", e))?;
        let removed = rotate(dir, self.cfg.generations)?;
        let state_copy = match &self.prepared.state_copy_dir {
            Some(s) => Some(crate::state_copy::write(self.db, s, &self.prepared.key).await?),
            None => None,
        };
        tracing::info!(
            target: "forge_cli",
            event = "forge.backup.completed",
            request_id = %self.request_id,
            generation = %generation,
            db_label = %manifest.db.label,
            repositories = manifest.repositories.len(),
            archive_bytes = manifest.archive.bytes,
            removed = removed.len(),
            state_copy = state_copy.is_some(),
            "backup completed"
        );
        Ok(Done {
            manifest,
            path: final_path,
            removed,
            state_copy,
        })
    }

    async fn fill(
        &self,
        generation: &str,
        partial: &Path,
        started_at: &str,
    ) -> Result<Manifest, ForgeError> {
        // 2. The database.
        let db_backup = self.tool.backup().await?;
        let after_db = forge_db::restore::clock(self.db).await?;
        tracing::info!(
            target: "forge_cli",
            event = "forge.backup.database_done",
            request_id = %self.request_id,
            generation,
            tool = db_backup.tool,
            label = %db_backup.label,
            started_at,
            "database backed up"
        );
        // 3. The repositories.
        let rows = forge_db::restore::repositories(self.db).await?;
        let staging = partial.join("staging");
        create_private_dir(&staging.join("repos"))
            .map_err(|e| io_failed("storage_io", "create_staging", e))?;
        let staging_root = StorageRoot::open(&staging)?;
        let file = create_private_file(&partial.join(ARCHIVE))
            .map_err(|e| io_failed("repository_copy_failed", "create_archive", e))?;
        let hashing = Hashing {
            inner: io::BufWriter::new(file),
            hasher: Sha256::new(),
            bytes: 0,
        };
        let sealed = crypt::SealWriter::new(hashing, &self.prepared.key, crypt::Kind::Repositories)
            .map_err(|e| io_failed("repository_copy_failed", "open_archive", e))?;
        let mut archive = archive::ArchiveWriter::new(sealed);
        let deadline = Instant::now() + self.cfg.repo_timeout;
        let mut repos = Vec::new();
        for row in rows.iter().filter(|r| r.ready) {
            let before = archive.bytes;
            self.copy_one(row.repo, &staging_root, &mut archive, deadline)
                .await?;
            repos.push(ManifestRepo {
                id: row.repo.get(),
                project_id: row.project.get(),
                bytes: archive.bytes - before,
            });
        }
        let sealed = archive
            .end()
            .map_err(|e| io_failed("repository_copy_failed", "end_archive", e))?;
        let hashing = sealed
            .finish()
            .map_err(|e| io_failed("repository_copy_failed", "finish_archive", e))?;
        let Hashing {
            inner,
            hasher,
            bytes,
        } = hashing;
        let file = inner
            .into_inner()
            .map_err(|e| io_failed("repository_copy_failed", "flush_archive", e.into_error()))?;
        file.sync_all()
            .map_err(|e| io_failed("repository_copy_failed", "sync_archive", e))?;
        fs::remove_dir_all(&staging).map_err(|e| io_failed("storage_io", "remove_staging", e))?;
        // 4. The manifest, last.
        let done_at = forge_db::restore::clock(self.db).await?;
        let manifest = Manifest {
            format: MANIFEST_FORMAT,
            generation: generation.to_owned(),
            request_id: self.request_id.to_string(),
            db: ManifestDb {
                tool: db_backup.tool.to_owned(),
                label: db_backup.label,
                completed_at: after_db.rfc3339,
                completed_at_micros: after_db.micros,
            },
            repositories: repos,
            archive: ManifestArchive {
                file: ARCHIVE.to_owned(),
                bytes,
                sha256: hex(&hasher.finalize()),
            },
            completed_at: done_at.rfc3339,
        };
        let json = serde_json::to_vec_pretty(&manifest)
            .map_err(|_| failed("storage_io").with_context("operation", "serialize_manifest"))?;
        let mut f = create_private_file(&partial.join(MANIFEST))
            .map_err(|e| io_failed("storage_io", "create_manifest", e))?;
        f.write_all(&json)
            .and_then(|()| f.sync_all())
            .map_err(|e| io_failed("storage_io", "write_manifest", e))?;
        sync_dir(partial).map_err(|e| io_failed("storage_io", "sync_dir", e))?;
        Ok(manifest)
    }

    /// Copy, check and append one repository (module docs, step 3).
    async fn copy_one<W: Write>(
        &self,
        repo: forge_core::ids::RepoId,
        staging: &StorageRoot,
        archive: &mut archive::ArchiveWriter<W>,
        deadline: Instant,
    ) -> Result<(), ForgeError> {
        // One alert code for every backup failure; the cause stays the source.
        let src = self.storage.existing(repo).map_err(|e| {
            failed("repository_copy_failed")
                .with_context("repo_id", repo.get())
                .with_context("operation", "open")
                .with_context("cause", e.code())
                .with_source(e)
        })?;
        let dst = staging.path_for(repo);
        let copy_failed = |op: &'static str, e: io::Error| {
            let reason = if e.kind() == io::ErrorKind::TimedOut {
                "repository_copy_timeout"
            } else {
                "repository_copy_failed"
            };
            io_failed(reason, op, e).with_context("repo_id", repo.get())
        };
        let mut attempt = 0;
        loop {
            attempt += 1;
            if dst.exists() {
                fs::remove_dir_all(&dst).map_err(|e| copy_failed("remove_copy", e))?;
            }
            archive::copy_repository(&src, &dst, deadline).map_err(|e| copy_failed("copy", e))?;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(failed("repository_copy_timeout").with_context("repo_id", repo.get()));
            }
            match self
                .git
                .fsck(staging, repo, FsckMode::Connectivity, left)
                .await
            {
                Ok(()) => break,
                Err(e) if e.code() == ErrorCode::RepositoryCorrupt && attempt < COPY_ATTEMPTS => {
                    tracing::warn!(
                        target: "forge_cli",
                        event = "forge.backup.copy_retried",
                        request_id = %self.request_id,
                        repo_id = repo.get(),
                        attempt,
                        error_code = %e.code(),
                        "the copied repository failed fsck; copying it again"
                    );
                }
                Err(e) if e.code() == ErrorCode::RepositoryCorrupt => {
                    return Err(failed("repository_copy_inconsistent")
                        .with_context("repo_id", repo.get())
                        .with_context("attempts", attempt)
                        .with_source(e));
                }
                Err(e) => {
                    return Err(failed("repository_copy_failed")
                        .with_context("repo_id", repo.get())
                        .with_context("operation", "fsck")
                        .with_context("cause", e.code())
                        .with_source(e))
                }
            }
        }
        archive
            .tree(staging.path(), &dst, deadline)
            .map_err(|e| copy_failed("archive", e))?;
        fs::remove_dir_all(&dst).map_err(|e| copy_failed("remove_copy", e))?;
        Ok(())
    }
}

/// Remove the `.partial` generations of runs that did not finish (the lock
/// is held, so none of them is running).
fn remove_partials(dir: &Path) -> Result<(), ForgeError> {
    let entries = fs::read_dir(dir).map_err(|e| io_failed("storage_io", "list", e))?;
    for e in entries {
        let e = e.map_err(|e| io_failed("storage_io", "list", e))?;
        let name = e.file_name();
        let is_partial = name
            .to_str()
            .and_then(|n| n.strip_suffix(PARTIAL))
            .is_some_and(is_generation_name);
        if is_partial {
            fs::remove_dir_all(e.path())
                .map_err(|e| io_failed("storage_io", "remove_partial", e))?;
            tracing::info!(
                target: "forge_cli",
                event = "forge.backup.partial_removed",
                name = %name.to_string_lossy(),
                "a generation of an unfinished run was removed"
            );
        }
    }
    Ok(())
}

/// Keep the newest `keep` complete generations; returns the removed names.
pub fn rotate(dir: &Path, keep: u32) -> Result<Vec<String>, ForgeError> {
    let all = generations(dir).map_err(|e| io_failed("storage_io", "list", e))?;
    let keep = usize::try_from(keep).unwrap_or(usize::MAX);
    let excess = all.len().saturating_sub(keep);
    let mut removed = Vec::new();
    for (name, path) in all.into_iter().take(excess) {
        fs::remove_dir_all(&path)
            .map_err(|e| io_failed("storage_io", "rotate", e).with_context("generation", &name))?;
        removed.push(name);
    }
    Ok(removed)
}

/// `forge backup` with the configured pgBackRest.
pub fn backup(cfg: &Config, request_id: &RequestId) -> Result<String, ForgeError> {
    let prepared = prepare(&cfg.backup)?;
    let storage = crate::worker::provision::Storage::open(cfg.storage_root()?)?;
    let tool = PgBackRest {
        bin: cfg.backup.pgbackrest_bin.clone(), // clone: owned by the tool
        stanza: cfg.backup.pgbackrest_stanza.clone(), // clone: owned by the tool
        timeout: cfg.backup.db_timeout,
    };
    crate::admin_token::with_db(&cfg.database, async |db| {
        let done = Run {
            db,
            cfg: &cfg.backup,
            prepared: &prepared,
            storage: &storage.root,
            git: &storage.git,
            tool: &tool,
            request_id,
        }
        .run()
        .await?;
        Ok(summary(&done))
    })
}

pub fn summary(done: &Done) -> String {
    let m = &done.manifest;
    let mut out = format!(
        "backup {} written to {}\n  database: {} {} (database clock {})\n  repositories: {} \
         ({} bytes encrypted, sha256 {})\n",
        m.generation,
        done.path.display(),
        m.db.tool,
        m.db.label,
        m.db.completed_at,
        m.repositories.len(),
        m.archive.bytes,
        m.archive.sha256
    );
    if !done.removed.is_empty() {
        out.push_str(&format!(
            "  removed old generations: {}\n",
            done.removed.join(", ")
        ));
    }
    match &done.state_copy {
        Some(p) => out.push_str(&format!("  state copy: {}\n", p.display())),
        None => out.push_str(
            "  state copy: not configured (backup.state_copy_dir): a restore will remove every \
             administrator flag\n",
        ),
    }
    out
}

#[cfg(test)]
pub(crate) mod tests;
