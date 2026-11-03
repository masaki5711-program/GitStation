//! Repository maintenance (M3 O3, plan 11 §5 O3, ADR-0024 "実装で決めたこと
//! （O3）"): the git half of the `repository.maintain` job. forge's
//! repositories run with `gc.auto=0` (`storage::REPO_SETTINGS`), so nothing
//! else ever prunes the unreferenced trees and commits that trial merges
//! (`merge-tree --write-tree`) and superseded merges (`commit-tree`) leave.
//!
//! The job runs these steps, each its own git call with its own timeout
//! and the **whole process tree** killed on a timeout or a drop
//! (`GitCommand::supervise_tree`: `gc` starts `repack` → `pack-objects`):
//!
//! 1. [`Git::maintenance_blocker`] — file-system checks first: an incoming
//!    push quarantine (`objects/tmp_objdir-incoming-*`, receive-pack's
//!    objects not yet migrated) younger than [`QUARANTINE_STALE`] defers
//!    the run ([`Blocked::IncomingPush`]); gc never starts during a push.
//!    Older ones are a crashed push's leftovers, removed by gc's own prune
//!    once past the grace.
//! 2. [`Git::gc_objects`] — `git -c gc.packRefs=false -c
//!    gc.writeCommitGraph=false -c repack.writeBitmaps=true -c
//!    pack.writeBitmapHashCache=true -c core.hooksPath=<empty> gc --cruft
//!    --prune=<grace>.seconds.ago --quiet`: one pack with bitmaps,
//!    unreachable objects in a cruft pack until they are older than the
//!    grace. Refs are NOT packed here (pack-refs takes the ref locks, so it
//!    is its own step under the merge lock), and the commit-graph is
//!    written by its own step (so each is bounded and reported alone). A gc
//!    already running (git's own `gc.pid` check: same host, live pid, at
//!    most 12 h old) is [`GcRun::AlreadyRunning`], never a failure.
//! 3. [`Git::pack_refs`] — `git pack-refs --all --prune`. The caller holds
//!    the repository's merge lock (`merge:<repo_id>`, try-lock: a merge in
//!    progress skips this step for the run).
//! 4. [`Git::write_commit_graph`] — `git commit-graph write --reachable`.
//! 5. [`Git::fsck`] — `git fsck --connectivity-only --no-dangling
//!    --no-progress` ([`FsckMode::Connectivity`], every run) or the full
//!    check ([`FsckMode::Full`], once per fsck interval). A finding is
//!    `FG-GIT-0019` RepositoryCorrupt (alert).
//! 6. [`Git::repository_counts`] — `count-objects -v` and the number of
//!    refs (`for-each-ref`, hidden merge request refs included: ADR-0024's
//!    open item on per-repository ref counts is measured here first).
//!
//! A step whose git exits non-zero (or prints what forge cannot read) is
//! `FG-GIT-0018` RepositoryMaintenanceFailed with `step`; a timeout stays
//! `FG-GIT-0002` and no free slot `FG-GIT-0004`, both with `step`.
//!
//! Ordering is the caller's: steps 3–6 run only when gc ran; a repository
//! whose fsck fails is never pruned again until it is clean (the job reads
//! the last outcome first).

use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};

use crate::command::Git;
use crate::storage::StorageRoot;
use crate::GitCommand;

/// The prefix of receive-pack's quarantine directories under `objects/`
/// (git's `tmp-objdir.c`: `tmp_objdir-incoming-XXXXXX`).
pub const QUARANTINE_PREFIX: &str = "tmp_objdir-incoming-";
/// A quarantine younger than this belongs to a push that may still be
/// running (the longest push is bounded by `git.http.timeout_ms`, 1 h by
/// default): maintenance is deferred. Older ones are a crashed push's.
pub const QUARANTINE_STALE: Duration = Duration::from_secs(6 * 3600);
/// The shortest prune grace forge accepts (plan 11 §5 O3, ADR-0024 §3): an
/// unreferenced object younger than this is never pruned.
pub const MIN_PRUNE_GRACE: Duration = Duration::from_secs(24 * 3600);
/// Captured output cap of the fsck and count steps (findings are cut to
/// this; a cut output is still a finding).
const STEP_OUTPUT_CAP: usize = 256 * 1024;
/// The `stdout` head of fsck kept for the log.
const FINDINGS_HEAD_LINES: usize = 20;

/// Why maintenance did not start (file-system checks, before any git).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// An incoming push's quarantine younger than [`QUARANTINE_STALE`].
    IncomingPush { quarantines: usize },
}

impl Blocked {
    /// The `skipped_reason` of the ledger and the logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IncomingPush { .. } => "incoming_push",
        }
    }
}

/// What [`Git::gc_objects`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcRun {
    /// gc ran to its end.
    Ran,
    /// Another gc holds the repository (`gc.pid`): nothing was done.
    AlreadyRunning,
}

/// Which `git fsck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsckMode {
    /// `--connectivity-only`: every reachable object exists (fast; with
    /// every maintenance run).
    Connectivity,
    /// The full check: every object is also read and validated (monthly).
    Full,
}

impl FsckMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connectivity => "connectivity",
            Self::Full => "full",
        }
    }
}

/// What `count-objects -v` and the ref count measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepositoryCounts {
    /// Loose objects (`count`).
    pub loose_objects: u64,
    /// Objects in packs (`in-pack`).
    pub packed_objects: u64,
    /// Packs (`packs`).
    pub packs: u64,
    /// The size of the packs in KiB (`size-pack`).
    pub size_pack_kib: u64,
    /// The size of the loose objects in KiB (`size`).
    pub size_loose_kib: u64,
    /// Files in `objects/` git does not recognise (`garbage`).
    pub garbage: u64,
    /// Every ref, hidden merge request refs included.
    pub refs: u64,
}

impl RepositoryCounts {
    /// Loose and packed objects (a packed object that is also loose counts
    /// twice, as in `count-objects`).
    pub fn objects(&self) -> u64 {
        self.loose_objects.saturating_add(self.packed_objects)
    }
}

/// The maintenance step names (`step` in every error and log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Gc,
    PackRefs,
    CommitGraph,
    Fsck,
    Counts,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gc => "gc",
            Self::PackRefs => "pack_refs",
            Self::CommitGraph => "commit_graph",
            Self::Fsck => "fsck",
            Self::Counts => "counts",
        }
    }
}

/// A step's failure: git's own failure becomes FG-GIT-0018 (with the exit
/// code and stderr it carried); timeouts, busy slots and storage errors keep
/// their code. Every one gets `step` and `repo_id`.
fn step_error(step: Step, id: RepoId, e: ForgeError) -> ForgeError {
    let e = if e.code() == ErrorCode::GitProcessFailed {
        let mut mapped = ForgeError::new(ErrorCode::RepositoryMaintenanceFailed);
        for (k, v) in e.context() {
            mapped = mapped.with_context(k, v);
        }
        mapped
    } else {
        e
    };
    e.with_context("step", step.as_str())
        .with_context("repo_id", id.get())
}

fn unexpected(step: Step, id: RepoId, subcommand: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RepositoryMaintenanceFailed)
        .with_context("reason", "unexpected_output")
        .with_context("subcommand", subcommand)
        .with_context("step", step.as_str())
        .with_context("repo_id", id.get())
}

/// Count the incoming quarantines under `objects/`: (younger than
/// [`QUARANTINE_STALE`], older).
fn scan_quarantines(objects: &Path, now: SystemTime) -> io::Result<(usize, usize)> {
    let mut fresh = 0;
    let mut stale = 0;
    for entry in std::fs::read_dir(objects)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(QUARANTINE_PREFIX) {
            continue;
        }
        // A quarantine that vanished meanwhile (its push finished) is gone.
        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        // A time in the future (clock step) counts as fresh: defer.
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age < QUARANTINE_STALE {
            fresh += 1;
        } else {
            stale += 1;
        }
    }
    Ok((fresh, stale))
}

impl Git {
    /// The file-system checks before maintenance (module docs, step 1).
    /// `Ok(None)`: go on. Errors: the storage's own (`FG-REPO-0001` /
    /// `0002` / `0003`).
    pub async fn maintenance_blocker(
        &self,
        storage: &StorageRoot,
        id: RepoId,
    ) -> Result<Option<Blocked>, ForgeError> {
        let repo = storage.existing(id)?;
        let objects = repo.join("objects");
        let scan =
            tokio::task::spawn_blocking(move || scan_quarantines(&objects, SystemTime::now()))
                .await
                .map_err(|join| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("operation", "maintenance_quarantine_scan")
                        .with_context("reason", "blocking_task_failed")
                        .with_source(join)
                })?;
        let (fresh, stale) = scan.map_err(|e| {
            ForgeError::new(ErrorCode::StorageIoFailed)
                .with_context("operation", "maintenance_quarantine_scan")
                .with_context("repo_id", id.get())
                .with_source(e)
        })?;
        if stale > 0 {
            tracing::info!(
                target: "forge_git::maintenance",
                repo_id = id.get(),
                stale_quarantines = stale,
                "forge.git.maintenance_stale_quarantine"
            );
        }
        Ok((fresh > 0).then_some(Blocked::IncomingPush { quarantines: fresh }))
    }

    /// The gc command of step 2 (module docs), for the argv test and
    /// [`Git::gc_objects`].
    pub(crate) fn gc_command(
        &self,
        repo: &Path,
        prune_grace: Duration,
        timeout: Duration,
    ) -> Result<GitCommand, ForgeError> {
        let cmd = self
            .command("gc")
            .config_override("gc.packRefs", "false")
            .config_override("gc.writeCommitGraph", "false")
            .config_override("repack.writeBitmaps", "true")
            .config_override("pack.writeBitmapHashCache", "true");
        Ok(self
            .with_empty_hooks(cmd)?
            .arg("--cruft")
            .prune_seconds_ago(prune_grace.as_secs())
            .arg("--quiet")
            .repo(repo)
            .timeout(timeout)
            .supervise_tree())
    }

    /// Step 2: `git gc --cruft --prune=<grace>` (module docs). A grace
    /// below [`MIN_PRUNE_GRACE`] is refused (`FG-SYS-0002`): forge never
    /// prunes an object a reconciliation may still need.
    pub async fn gc_objects(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        prune_grace: Duration,
        timeout: Duration,
    ) -> Result<GcRun, ForgeError> {
        if prune_grace < MIN_PRUNE_GRACE {
            return Err(ForgeError::new(ErrorCode::ConfigInvalid)
                .with_context("config_key", "maintenance.prune_grace_hours")
                .with_context("reason", "below_min"));
        }
        let repo = storage.existing(id)?;
        let cmd = self.gc_command(&repo, prune_grace, timeout)?;
        // 128: git's `die`, which is also how a running gc refuses.
        let run = cmd
            .output_accepting(STEP_OUTPUT_CAP, &[0, 128])
            .await
            .map_err(|e| step_error(Step::Gc, id, e))?;
        match run.exit_code {
            Some(0) => Ok(GcRun::Ran),
            Some(_) if run.stderr.contains("gc is already running") => Ok(GcRun::AlreadyRunning),
            code => Err(ForgeError::new(ErrorCode::RepositoryMaintenanceFailed)
                .with_context("subcommand", "gc")
                .with_context(
                    "exit_code",
                    code.map_or_else(|| "truncated".to_owned(), |c| c.to_string()),
                )
                .with_context("stderr", &run.stderr)
                .with_context("step", Step::Gc.as_str())
                .with_context("repo_id", id.get())),
        }
    }

    /// The pack-refs command of step 3.
    pub(crate) fn pack_refs_command(
        &self,
        repo: &Path,
        timeout: Duration,
    ) -> Result<GitCommand, ForgeError> {
        let cmd = self.command("pack-refs");
        Ok(self
            .with_empty_hooks(cmd)?
            .arg("--all")
            .arg("--prune")
            .repo(repo)
            .timeout(timeout)
            .supervise_tree())
    }

    /// Step 3: `git pack-refs --all --prune`. The caller holds the merge
    /// lock of the repository (module docs).
    pub async fn pack_refs(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        timeout: Duration,
    ) -> Result<(), ForgeError> {
        let repo = storage.existing(id)?;
        self.pack_refs_command(&repo, timeout)?
            .output()
            .await
            .map_err(|e| step_error(Step::PackRefs, id, e))?;
        Ok(())
    }

    /// The commit-graph command of step 4.
    pub(crate) fn commit_graph_command(
        &self,
        repo: &Path,
        timeout: Duration,
    ) -> Result<GitCommand, ForgeError> {
        let cmd = self.command("commit-graph");
        Ok(self
            .with_empty_hooks(cmd)?
            .arg("write")
            .arg("--reachable")
            .arg("--no-progress")
            .repo(repo)
            .timeout(timeout)
            .supervise_tree())
    }

    /// Step 4: `git commit-graph write --reachable`.
    pub async fn write_commit_graph(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        timeout: Duration,
    ) -> Result<(), ForgeError> {
        let repo = storage.existing(id)?;
        self.commit_graph_command(&repo, timeout)?
            .output()
            .await
            .map_err(|e| step_error(Step::CommitGraph, id, e))?;
        Ok(())
    }

    /// The fsck command of step 5.
    pub(crate) fn fsck_command(
        &self,
        repo: &Path,
        mode: FsckMode,
        timeout: Duration,
    ) -> Result<GitCommand, ForgeError> {
        let cmd = self.command("fsck");
        let cmd = match mode {
            FsckMode::Connectivity => cmd.arg("--connectivity-only"),
            FsckMode::Full => cmd.arg("--full"),
        };
        Ok(self
            .with_empty_hooks(cmd)?
            .arg("--no-dangling")
            .arg("--no-progress")
            .repo(repo)
            .timeout(timeout)
            .supervise_tree())
    }

    /// Step 5: `git fsck` in `mode`. A finding (fsck exits non-zero, git's
    /// own fatal errors included — they mean an unreadable repository) is
    /// `FG-GIT-0019` RepositoryCorrupt with `mode`, `exit_code`,
    /// `findings`, `first_finding` and the heads of stdout / stderr.
    pub async fn fsck(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        mode: FsckMode,
        timeout: Duration,
    ) -> Result<(), ForgeError> {
        let repo = storage.existing(id)?;
        let accepted: Vec<i32> = (0..=255).collect();
        let run = self
            .fsck_command(&repo, mode, timeout)?
            .output_accepting(STEP_OUTPUT_CAP, &accepted)
            .await
            .map_err(|e| step_error(Step::Fsck, id, e))?;
        let stdout = run.output.stdout_str();
        let lines: Vec<&str> = stdout
            .lines()
            .chain(run.stderr.split(" | "))
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if run.exit_code == Some(0) && !run.truncated {
            return Ok(());
        }
        let head: Vec<&str> = stdout.lines().take(FINDINGS_HEAD_LINES).collect();
        Err(ForgeError::new(ErrorCode::RepositoryCorrupt)
            .with_context("mode", mode.as_str())
            .with_context(
                "exit_code",
                run.exit_code
                    .map_or_else(|| "truncated".to_owned(), |c| c.to_string()),
            )
            .with_context("findings", lines.len())
            .with_context("first_finding", lines.first().copied().unwrap_or(""))
            .with_context("stdout", head.join(" | "))
            .with_context("stderr", &run.stderr)
            .with_context("step", Step::Fsck.as_str())
            .with_context("repo_id", id.get()))
    }

    /// Step 6: the object and ref counts.
    pub async fn repository_counts(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        timeout: Duration,
    ) -> Result<RepositoryCounts, ForgeError> {
        let repo = storage.existing(id)?;
        let out = self
            .command("count-objects")
            .arg("-v")
            .repo(&repo)
            .timeout(timeout)
            .output()
            .await
            .map_err(|e| step_error(Step::Counts, id, e))?;
        let mut counts = parse_count_objects(&out.stdout_str())
            .ok_or_else(|| unexpected(Step::Counts, id, "count-objects"))?;
        // One byte per ref (`.` and a newline): 1 000 000 refs fit the cap.
        let refs = self
            .command("for-each-ref")
            .arg("--format=.")
            .repo(&repo)
            .timeout(timeout)
            .output()
            .await
            .map_err(|e| step_error(Step::Counts, id, e))?;
        counts.refs = u64::try_from(refs.stdout.iter().filter(|b| **b == b'\n').count())
            .map_err(|_| unexpected(Step::Counts, id, "for-each-ref"))?;
        Ok(counts)
    }
}

/// `count-objects -v`: `key: value` lines; the keys forge reads must all
/// be there (a git that renamed one is an unexpected output, never a 0).
pub(crate) fn parse_count_objects(text: &str) -> Option<RepositoryCounts> {
    let mut found = [None::<u64>; 6];
    const KEYS: [&str; 6] = ["count", "in-pack", "packs", "size-pack", "size", "garbage"];
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if let Some(i) = KEYS.iter().position(|k| *k == key.trim()) {
            found[i] = Some(value.trim().parse().ok()?);
        }
    }
    let [Some(loose), Some(packed), Some(packs), Some(size_pack), Some(size_loose), Some(garbage)] =
        found
    else {
        return None;
    };
    Some(RepositoryCounts {
        loose_objects: loose,
        packed_objects: packed,
        packs,
        size_pack_kib: size_pack,
        size_loose_kib: size_loose,
        garbage,
        refs: 0,
    })
}

#[cfg(test)]
#[path = "maintenance_tests.rs"]
mod tests;
