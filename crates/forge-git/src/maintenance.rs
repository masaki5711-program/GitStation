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
//!    The job checks it again right before gc (fsck may have taken long).
//!    Older ones are a crashed push's leftovers: they do not defer, and
//!    gc's own prune removes them — directory and files — once they are
//!    older than the prune grace (verified on git 2.53,
//!    `a_stale_quarantine_with_files_is_pruned_after_the_grace`).
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
//!    This process's long-lived `cat-file` processes of the repository are
//!    closed first (`browse::release_repository`); a pack git could not
//!    delete because another process holds it is retried once after
//!    [`FILES_IN_USE_RETRY`] and otherwise said once (FG-GIT-0032
//!    `reason=files_in_use`): the next gc removes it.
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
//! Ordering is the caller's. The job (`forge worker`, `worker/maintenance.rs`)
//! runs the blocker check, then fsck **before** gc (a finding stops the run,
//! so a corrupt repository is never pruned; the next run's fsck is full),
//! then gc, pack-refs, commit-graph and the counts.

use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};

use crate::command::Git;
use crate::storage::StorageRoot;
use crate::GitCommand;

/// How long gc waits before its one retry when a file was in use: a
/// `forge serve` process closes an idle batch process after
/// `BrowseLimits::batch_idle` (60 s), its sweep runs every 30 s.
pub const FILES_IN_USE_RETRY: Duration = Duration::from_secs(95);

/// The time a gc retry needs at least to be worth starting.
const MIN_GC_RETRY_RUN: Duration = Duration::from_secs(30);

/// A gc that met a file in use: it ran and said so, or it failed saying so.
fn gc_hit_files_in_use(outcome: &Result<(GcRun, Option<String>), ForgeError>) -> bool {
    match outcome {
        Ok((_, in_use)) => in_use.is_some(),
        Err(e) => {
            e.code() == ErrorCode::RepositoryMaintenanceFailed
                && e.context()
                    .iter()
                    .any(|(k, v)| *k == "stderr" && files_in_use(v))
        }
    }
}

/// Whether git's stderr says a file could not be deleted because it is in
/// use (Git for Windows: `Unlink of file '…' failed`, `unable to unlink
/// '…': Permission denied`, `Deletion of directory '…' failed`).
pub(crate) fn files_in_use(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    [
        "unable to unlink",
        "unlink of file",
        "deletion of directory",
        "being used by another process",
        "sharing violation",
    ]
    .iter()
    .any(|p| s.contains(p))
}

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
        self.gc_retrying(storage, id, prune_grace, timeout, FILES_IN_USE_RETRY)
            .await
    }

    /// [`Self::gc_objects`] with its wait before the retry (tests). This
    /// process's batch processes of the repository are closed first; when
    /// git still says a file was in use (an old pack it could not delete:
    /// another process holds it, e.g. a `forge serve` batch process on a
    /// Windows git without delete sharing) gc runs once more after
    /// `retry_after` (when the time left allows), and a pack still left
    /// behind is said once — FG-GIT-0032 `reason=files_in_use`, warn — and
    /// the run goes on (the next gc removes it).
    pub(crate) async fn gc_retrying(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        prune_grace: Duration,
        timeout: Duration,
        retry_after: Duration,
    ) -> Result<GcRun, ForgeError> {
        let started = std::time::Instant::now();
        let repo = storage.existing(id)?;
        crate::browse::release_repository_async(storage, id).await;
        let mut outcome = self.gc_once(&repo, id, prune_grace, timeout).await;
        let mut retried = false;
        if gc_hit_files_in_use(&outcome) {
            let left = timeout.saturating_sub(started.elapsed());
            if left > retry_after + MIN_GC_RETRY_RUN {
                tokio::time::sleep(retry_after).await;
                crate::browse::release_repository_async(storage, id).await;
                retried = true;
                outcome = self
                    .gc_once(&repo, id, prune_grace, left - retry_after)
                    .await;
            }
        }
        match outcome {
            Ok((ran, None)) => Ok(ran),
            Ok((ran, Some(stderr))) => {
                tracing::warn!(
                    target: "forge_git::maintenance",
                    error_code = ErrorCode::GitBatchFailed.id(),
                    reason = "files_in_use",
                    step = Step::Gc.as_str(),
                    repo_id = id.get(),
                    retried,
                    stderr = %stderr,
                    "forge.git.maintenance.files_in_use"
                );
                Ok(ran)
            }
            // Still failing: the job logs it once (FG-GIT-0018, its stderr).
            Err(e) if retried => Err(e.with_context("retried_after_s", retry_after.as_secs())),
            Err(e) => Err(e),
        }
    }

    /// One gc; the stderr when git said a file was in use.
    async fn gc_once(
        &self,
        repo: &Path,
        id: RepoId,
        prune_grace: Duration,
        timeout: Duration,
    ) -> Result<(GcRun, Option<String>), ForgeError> {
        let cmd = self.gc_command(repo, prune_grace, timeout)?;
        // 128: git's `die`, which is also how a running gc refuses.
        let run = cmd
            .output_accepting(STEP_OUTPUT_CAP, &[0, 128])
            .await
            .map_err(|e| step_error(Step::Gc, id, e))?;
        let in_use = files_in_use(&run.stderr).then(|| run.stderr.clone()); // clone: logged, the run is matched below
        let ran = match run.exit_code {
            Some(0) => GcRun::Ran,
            Some(_) if run.stderr.contains("gc is already running") => GcRun::AlreadyRunning,
            code => {
                return Err(ForgeError::new(ErrorCode::RepositoryMaintenanceFailed)
                    .with_context("subcommand", "gc")
                    .with_context(
                        "exit_code",
                        code.map_or_else(|| "truncated".to_owned(), |c| c.to_string()),
                    )
                    .with_context("stderr", &run.stderr)
                    .with_context("step", Step::Gc.as_str())
                    .with_context("repo_id", id.get()))
            }
        };
        Ok((ran, in_use))
    }

    /// The pack-refs command of step 3.
    pub(crate) fn pack_refs_command(
        &self,
        repo: &Path,
        timeout: Duration,
    ) -> Result<GitCommand, ForgeError> {
        // Its own wait for `packed-refs.lock` (a push deleting a packed ref
        // holds it for a moment) is the writers' 10 s too, not git's 1 s.
        let cmd = self.command("pack-refs").config_override(
            "core.packedRefsTimeout",
            crate::refs::PACKED_REFS_TIMEOUT_MS,
        );
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
        // git could not look at the repository at all (not one, owned by
        // someone else): an operational failure, never a finding about the
        // objects.
        if run.exit_code == Some(128) && fsck_unusable(&run.stderr) {
            return Err(ForgeError::new(ErrorCode::RepositoryMaintenanceFailed)
                .with_context("reason", "fsck_unusable")
                .with_context("subcommand", "fsck")
                .with_context("exit_code", 128)
                .with_context("stderr", &run.stderr)
                .with_context("step", Step::Fsck.as_str())
                .with_context("repo_id", id.get()));
        }
        // The head of stdout is redacted like stderr (object ids and ref
        // names stay; URL credentials or secret-looking values would not).
        let head = crate::redact::stderr_head(
            stdout
                .lines()
                .take(FINDINGS_HEAD_LINES)
                .collect::<Vec<_>>()
                .join("\n")
                .as_bytes(),
        );
        let first = crate::redact::stderr_head(lines.first().copied().unwrap_or("").as_bytes());
        Err(ForgeError::new(ErrorCode::RepositoryCorrupt)
            .with_context("mode", mode.as_str())
            .with_context(
                "exit_code",
                run.exit_code
                    .map_or_else(|| "truncated".to_owned(), |c| c.to_string()),
            )
            .with_context("findings", lines.len())
            .with_context("first_finding", first)
            .with_context("stdout", head)
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

/// git's words for "this is not a repository I may read" (LC_ALL=C).
fn fsck_unusable(stderr: &str) -> bool {
    stderr.contains("fatal:")
        && [
            "not a git repository",
            "dubious ownership",
            "safe.directory",
        ]
        .iter()
        .any(|m| stderr.contains(m))
}

/// How a maintenance step's git ended, from its error (`stop`,
/// `killed_at_unix_ms` of a timed-out supervised run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepEnd {
    /// git exited by itself (any exit code): it removed its own locks.
    Exited,
    /// Stopped by SIGTERM within the grace: git removed its own locks.
    Terminated,
    /// SIGKILL / the Job Object closed at `at`: its locks may be left.
    Killed { at: SystemTime },
    /// No git ran to its end or was stopped (storage, slot, deadline before
    /// the start): nothing of ours to clean.
    NotRun,
}

impl StepEnd {
    /// Classify a step's error.
    pub fn of(err: &ForgeError) -> Self {
        let ctx = |key: &str| {
            err.context()
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.as_str())
        };
        match (err.code(), ctx("stop")) {
            (ErrorCode::GitTimeout, Some("term")) => Self::Terminated,
            (ErrorCode::GitTimeout, Some("kill")) => {
                let at = ctx("killed_at_unix_ms")
                    .and_then(|ms| ms.parse::<u64>().ok())
                    .map_or_else(SystemTime::now, |ms| {
                        SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
                    });
                Self::Killed { at }
            }
            (ErrorCode::GitTimeout, _) => Self::NotRun,
            (ErrorCode::RepositoryMaintenanceFailed | ErrorCode::GitProcessFailed, _)
                if ctx("exit_code").is_some() =>
            {
                Self::Exited
            }
            _ => Self::NotRun,
        }
    }
}

/// What [`Git::clear_own_packed_refs_lock`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockCleanup {
    /// No `packed-refs.lock`.
    Absent,
    /// Ours (created at or after the step started): removed.
    Removed,
    /// Older than the step: someone else's, left in place.
    Foreign { age_secs: u64 },
}

impl LockCleanup {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Removed => "removed",
            Self::Foreign { .. } => "foreign",
        }
    }
}

/// The age of `<repo>/packed-refs.lock`, `None` when there is none
/// (doctor `repositories.stale_locks`). Blocking (`std::fs`).
pub fn packed_refs_lock_age(
    storage: &StorageRoot,
    id: RepoId,
    now: SystemTime,
) -> Result<Option<Duration>, ForgeError> {
    let repo = storage.existing(id)?;
    match std::fs::metadata(repo.join("packed-refs.lock")).and_then(|m| m.modified()) {
        Ok(t) => Ok(Some(now.duration_since(t).unwrap_or(Duration::ZERO))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ForgeError::new(ErrorCode::StorageIoFailed)
            .with_context("operation", "packed_refs_lock_stat")
            .with_context("repo_id", id.get())
            .with_source(e)),
    }
}

/// How much older than the step's start our own `packed-refs.lock` may look:
/// file times are stamped from a coarse clock, the step's start from a
/// fine-grained one. The kernel tick alone gives a few milliseconds (measured
/// up to 8 ms), but some file systems keep whole seconds (ext2/ext3; two on
/// FAT), and the test and production file systems are not fixed — so 2 s, a
/// deliberate trade-off. It widens the residual window below by 2 s.
const LOCK_MTIME_SLACK: Duration = Duration::from_secs(2);

impl Git {
    /// After a pack-refs that was **killed** ([`StepEnd::Killed`]: git
    /// could not remove its own lock), still under the merge lock: remove
    /// `packed-refs.lock` only when its time lies between `step_started`
    /// (less [`LOCK_MTIME_SLACK`] for coarse file times) and `killed_at` —
    /// it can then only be the one our pack-refs made. `killed_at` is when
    /// the kill was sent, carried in whole milliseconds rounded up: file
    /// times may be finer than a millisecond (Linux multigrain timestamps
    /// once anyone, e.g. the doctor's probe, stat()s the lock), and our git's
    /// last write in the kill's millisecond must still count as ours. The
    /// rounding lets a lock made up to 1 ms after the kill count as ours —
    /// only possible when our git had not taken the lock (O_EXCL), the
    /// residual below. Not covered, by design (2026-11-09): a git blocked in
    /// the kernel (slow storage) that finishes a write to its lock after the
    /// kill was sent — its lock looks newer than the kill and is left and
    /// named (a false FG-GIT-0020, the safe side); moving the bound later
    /// would let a live writer's lock be removed instead.
    /// Anything else is left ([`LockCleanup::Foreign`]: the caller alerts,
    /// FG-GIT-0020): the merge lock does **not** keep writers out —
    /// receive-pack and `update-ref` of the version job take
    /// `packed-refs.lock` without it — so a lock made after the kill belongs
    /// to a live writer. Never call it after a pack-refs that exited on its
    /// own or after SIGTERM (git removed its own lock then; whatever lock
    /// exists is someone else's). Residual: a writer that took the lock
    /// while our pack-refs waited for it (≤ 10 s) and before the kill has a
    /// time inside the window; on Unix the kill comes only after pack-refs
    /// ignored SIGTERM for 5 s, on Windows (development only) at once. The
    /// slack adds one more: a writer that took the lock up to 2 s before the
    /// step and still holds it at the kill (pack-refs waits ≤ 10 s for it,
    /// the kill comes ≥ 5 s in) — very unlikely, accepted (2026-11-08).
    pub async fn clear_own_packed_refs_lock(
        &self,
        storage: &StorageRoot,
        id: RepoId,
        step_started: SystemTime,
        killed_at: SystemTime,
    ) -> Result<LockCleanup, ForgeError> {
        let repo = storage.existing(id)?;
        let lock = repo.join("packed-refs.lock");
        let io_failed = move |e: io::Error| {
            ForgeError::new(ErrorCode::StorageIoFailed)
                .with_context("operation", "packed_refs_lock_cleanup")
                .with_context("repo_id", id.get())
                .with_source(e)
        };
        let done = tokio::task::spawn_blocking(move || {
            let modified = match std::fs::metadata(&lock).and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LockCleanup::Absent),
                Err(e) => return Err(e),
            };
            // File times come from a coarse clock (the kernel tick, or whole
            // seconds on some file systems); `step_started` is fine-grained,
            // so our own lock can look up to that much older than the step.
            let earliest = step_started
                .checked_sub(LOCK_MTIME_SLACK)
                .unwrap_or(step_started);
            if modified >= earliest && modified <= killed_at {
                match std::fs::remove_file(&lock) {
                    Ok(()) => Ok(LockCleanup::Removed),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(LockCleanup::Absent),
                    Err(e) => Err(e),
                }
            } else {
                let age = SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or(Duration::ZERO);
                Ok(LockCleanup::Foreign {
                    age_secs: age.as_secs(),
                })
            }
        })
        .await
        .map_err(|join| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "packed_refs_lock_cleanup")
                .with_context("reason", "blocking_task_failed")
                .with_source(join)
        })?;
        done.map_err(io_failed)
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
