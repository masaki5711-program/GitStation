//! Long-lived `git cat-file --batch-command --buffer` processes, one small
//! pool per repository (the browse read path; FG-GIT-0032).
//!
//! Starting git costs ~20–40 ms on Windows, and one browse request used to
//! start two or three. Objects are immutable by id, so a process that stays
//! up and answers `info <oid>` / `contents <oid>` for many requests returns
//! exactly what a fresh `cat-file` would. **Refs are never read here**
//! (they move): only full object ids a caller resolved before.
//!
//! - **Same isolation as every git**: the process is built by
//!   [`crate::GitCommand::build`] (cleared environment, forge-owned config,
//!   `GIT_DIR` pinned to the validated repository path, no console window
//!   on Windows, killed on drop); only its working directory is forge's
//!   home instead of the repository, so a long-lived process never keeps
//!   the repository directory from being renamed or deleted on Windows.
//! - **Files rewritten or removed**: [`close_everywhere`] closes every
//!   process of a repository in this process and waits (bounded) for each
//!   to exit; storage removal and gc call it first
//!   (`browse::release_repository`).
//! - **Unsupported git** (no `--batch-command`, < 2.36: below forge's
//!   minimum, so only a misconfigured `git.bin`): the first process exits
//!   with git's usage status 129; the pool turns itself off, warns once
//!   (FG-GIT-0032 `reason=unsupported`) and every read is a one-shot git. Its stdin carries only `info` /
//!   `contents` lines with a validated 40 / 64-hex id and `flush`; nothing
//!   a user typed reaches it. stderr is discarded (a failure is answered
//!   again by the one-shot command, whose stderr is logged).
//! - **Bounds**: at most `per_repo` processes per repository (a semaphore;
//!   a caller waits at most `wait`, then the caller falls back), at most
//!   `max_repos` repositories (the one used longest ago with no process
//!   busy is closed first; when every one is busy the caller falls back).
//!   A process unused for `idle` is closed by a background sweep. The
//!   processes do not take a slot of the global git pool (they are bounded
//!   by `per_repo × max_repos` on their own and would otherwise hold slots
//!   while idle). Memory: each process holds at most
//!   `git.pack.delta_base_cache_mib` of inflated bases plus its mapped pack
//!   windows (≤ `git.pack.packed_git_limit_mib`, file-backed: the page cache
//!   reclaims them), so the pool's worst case is `per_repo × max_repos ×
//!   (delta_base_cache + a few MiB)` (`GitConfig::pack`,
//!   `ops/runbooks/memory-sizing.md`).
//! - **Every exchange has a timeout** (the caller's, never past its
//!   operation's deadline). A timeout, an I/O error (the process died, its
//!   output ended mid-answer) or an answer out of protocol kills the
//!   process (it is never reused: a [`Lease`] is returned to the pool only
//!   after a complete exchange) and is reported as
//!   [`BatchError::Failed`] (FG-GIT-0032); the caller logs it once and
//!   answers with the one-shot command. A full pool is
//!   [`BatchError::Busy`]: not a failure, the caller falls back.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::command::Git;

/// The longest header line forge accepts (`<64 hex> <type> <size>`).
const MAX_HEADER_BYTES: usize = 256;

/// The pool's bounds (from `BrowseLimits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BatchConfig {
    /// Processes per repository (≥ 1; a pool is not built for 0).
    pub(crate) per_repo: usize,
    /// Repositories with processes at once.
    pub(crate) max_repos: usize,
    /// A process unused this long is closed.
    pub(crate) idle: Duration,
    /// How long a caller waits for a busy repository's process.
    pub(crate) wait: Duration,
}

/// What `info <oid>` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Info {
    Found { kind: String, size: u64 },
    Missing,
}

/// What `contents <oid>` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Contents {
    Found {
        kind: String,
        data: Vec<u8>,
    },
    Missing,
    /// Larger than the caller's cap: not read (`info` only).
    TooLarge {
        kind: String,
        size: u64,
    },
}

/// Why a batch read was not answered.
#[derive(Debug)]
pub(crate) enum BatchError {
    /// No process in time (the repository's processes busy, the pool
    /// full): fall back, nothing failed.
    Busy,
    /// The process failed (FG-GIT-0032, `reason`): killed; fall back and
    /// log this once.
    Failed(ForgeError),
}

fn failed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::GitBatchFailed)
        .with_context("subcommand", "cat-file")
        .with_context("reason", reason)
}

fn protocol(detail: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail)
}

/// The pool (cheap to clone).
#[derive(Debug, Clone)]
pub(crate) struct CatFilePool {
    inner: Arc<PoolInner>,
}

/// Every pool of this process, so a removal or a rewrite of a
/// repository's files can close its processes wherever they run in this
/// process ([`close_everywhere`]).
static POOLS: Mutex<Vec<Weak<PoolInner>>> = Mutex::new(Vec::new());

#[derive(Debug)]
struct PoolInner {
    git: Git,
    cfg: BatchConfig,
    repos: Mutex<HashMap<PathBuf, RepoProcs>>,
    /// Killed processes not yet seen to exit (reaped by the sweep and by
    /// [`close_everywhere`], which waits for them).
    dying: Mutex<Vec<Dying>>,
    sweeping: AtomicBool,
    /// Latched on when git does not know `--batch-command` (< 2.36): every
    /// read is then a one-shot git, said once.
    unsupported: AtomicBool,
}

#[derive(Debug)]
struct RepoProcs {
    /// One per process slot; also the identity of this entry (a lease of
    /// a closed entry never returns its process to a new one).
    permits: Arc<Semaphore>,
    idle: Vec<IdleProc>,
    used: Instant,
}

#[derive(Debug)]
struct IdleProc {
    proc: BatchProc,
    since: Instant,
}

#[derive(Debug)]
struct Dying {
    dir: PathBuf,
    proc: BatchProc,
}

/// One running `cat-file --batch-command --buffer`.
#[derive(Debug)]
pub(crate) struct BatchProc {
    // Also killed on drop (`kill_on_drop`, set by the command builder).
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl BatchProc {
    /// Stop it (no wait); a process already gone is fine.
    fn kill(&mut self) {
        let _ = self.child.start_kill(); // ignore: it may have exited already
    }

    /// Whether it has exited (an error asking counts as gone: there is
    /// nothing more to wait for).
    fn exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }
}

impl CatFilePool {
    pub(crate) fn new(git: Git, cfg: BatchConfig) -> Self {
        let inner = Arc::new(PoolInner {
            git,
            cfg,
            repos: Mutex::new(HashMap::new()),
            dying: Mutex::new(Vec::new()),
            sweeping: AtomicBool::new(false),
            unsupported: AtomicBool::new(false),
        });
        let mut pools = POOLS.lock().unwrap_or_else(PoisonError::into_inner);
        pools.retain(|p| p.strong_count() > 0);
        pools.push(Arc::downgrade(&inner));
        Self { inner }
    }

    /// `info` for each of `oids` (full ids) in one exchange.
    pub(crate) async fn info(
        &self,
        dir: &Path,
        oids: &[&str],
        timeout: Duration,
    ) -> Result<Vec<Info>, BatchError> {
        let mut lease = self.lease(dir, timeout).await?;
        let proc = lease.proc()?;
        let answered = tokio::time::timeout(timeout, proc.info(oids)).await;
        match lease.finish(answered, timeout) {
            Ok(v) => Ok(v),
            Err(e) => Err(self.after_failure(&mut lease, e).await),
        }
    }

    /// `contents` of `oid` (a full id) when it is at most `max` bytes.
    pub(crate) async fn contents(
        &self,
        dir: &Path,
        oid: &str,
        max: u64,
        timeout: Duration,
    ) -> Result<Contents, BatchError> {
        let mut lease = self.lease(dir, timeout).await?;
        let proc = lease.proc()?;
        let answered = tokio::time::timeout(timeout, proc.contents(oid, max)).await;
        match lease.finish(answered, timeout) {
            Ok(v) => Ok(v),
            Err(e) => Err(self.after_failure(&mut lease, e).await),
        }
    }

    /// A process that failed on its first exchange and exited with git's
    /// usage status (129): this git has no `--batch-command`. The pool
    /// turns itself off, says so once (FG-GIT-0032 `reason=unsupported`)
    /// and every later read is a one-shot git without another warning.
    async fn after_failure(&self, lease: &mut Lease, err: BatchError) -> BatchError {
        if !lease.fresh {
            return err;
        }
        let Some(proc) = lease.proc.as_mut() else {
            return err;
        };
        let mut status = None;
        for _ in 0..50 {
            match proc.child.try_wait() {
                Ok(Some(s)) => {
                    status = Some(s);
                    break;
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(_) => break,
            }
        }
        if status.and_then(|s| s.code()) != Some(129) {
            return err;
        }
        if !self.inner.unsupported.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                target: "forge_git::batch",
                error_code = ErrorCode::GitBatchFailed.id(),
                reason = "unsupported",
                git_bin = %self.inner.git.config().bin.display(),
                "forge.git.batch.unsupported"
            );
        }
        BatchError::Busy
    }

    /// Turned off for good (tests).
    #[cfg(test)]
    pub(crate) fn is_unsupported(&self) -> bool {
        self.inner.unsupported.load(Ordering::Acquire)
    }

    /// A process of `dir`: an idle one, else a new one (within the bounds).
    async fn lease(&self, dir: &Path, timeout: Duration) -> Result<Lease, BatchError> {
        if self.inner.unsupported.load(Ordering::Acquire) {
            return Err(BatchError::Busy);
        }
        self.start_sweep();
        let cfg = self.inner.cfg;
        let permits = {
            let mut repos = self.lock();
            if !repos.contains_key(dir) && repos.len() >= cfg.max_repos.max(1) {
                // Close the repository used longest ago that has nothing busy.
                let victim = repos
                    .iter()
                    .filter(|(_, r)| Arc::strong_count(&r.permits) == 1)
                    .min_by_key(|(_, r)| r.used)
                    .map(|(k, _)| k.clone()); // clone: the key outlives the borrow
                let Some(v) = victim else {
                    return Err(BatchError::Busy);
                };
                if let Some(old) = repos.remove(&v) {
                    bury(&self.inner, &v, old.idle.into_iter().map(|i| i.proc));
                }
            }
            let entry = repos.entry(dir.to_path_buf()).or_insert_with(|| RepoProcs {
                permits: Arc::new(Semaphore::new(cfg.per_repo)),
                idle: Vec::new(),
                used: Instant::now(),
            });
            entry.used = Instant::now();
            Arc::clone(&entry.permits)
        };
        let wait = cfg.wait.min(timeout);
        let permit = match tokio::time::timeout(wait, Arc::clone(&permits).acquire_owned()).await {
            Ok(Ok(p)) => p,
            // Timed out, or the repository was closed meanwhile.
            Ok(Err(_)) | Err(_) => return Err(BatchError::Busy),
        };
        let reused = {
            let mut repos = self.lock();
            repos
                .get_mut(dir)
                .filter(|r| Arc::ptr_eq(&r.permits, &permits))
                .and_then(|r| r.idle.pop())
                .map(|i| i.proc)
        };
        let (proc, fresh) = match reused {
            Some(p) => (p, false),
            None => (self.spawn(dir).await.map_err(BatchError::Failed)?, true),
        };
        Ok(Lease {
            pool: Arc::clone(&self.inner),
            dir: dir.to_path_buf(),
            permits,
            proc: Some(proc),
            healthy: false,
            fresh,
            _permit: permit,
        })
    }

    /// Start a process (on the blocking pool: creating a process takes
    /// ~30 ms on Windows and must not stall the async workers).
    async fn spawn(&self, dir: &Path) -> Result<BatchProc, ForgeError> {
        let command = self
            .inner
            .git
            .command("cat-file")
            .arg("--batch-command")
            .arg("--buffer")
            .repo(dir);
        let mut cmd = command.build(true);
        cmd.stderr(Stdio::null());
        // `GIT_DIR` pins the repository; the working directory is forge's
        // own home, not the repository: on Windows a process whose working
        // directory is inside a directory blocks renaming or deleting it
        // (os error 32, measured on git 2.53), and these processes live
        // for a minute. Open pack files do not block (git opens them with
        // delete sharing).
        cmd.current_dir(&self.inner.git.config().home);
        let spawned = tokio::task::spawn_blocking(move || cmd.spawn())
            .await
            .map_err(|e| failed("spawn_failed").with_source(e))?;
        let mut child = spawned.map_err(|e| {
            failed("spawn_failed")
                .with_context("git_bin", self.inner.git.config().bin.display())
                .with_source(e)
        })?;
        crate::metrics::spawned("cat-file", "batch");
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(failed("pipes_missing"));
        };
        Ok(BatchProc {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, RepoProcs>> {
        self.inner
            .repos
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The background sweep that closes idle processes (one per pool,
    /// started on first use; it ends when the pool is dropped).
    fn start_sweep(&self) {
        if self.inner.sweeping.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak: Weak<PoolInner> = Arc::downgrade(&self.inner);
        let every = (self.inner.cfg.idle / 2).max(Duration::from_millis(50));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                sweep(&inner);
            }
        });
    }

    /// Close every process of `dir` in this pool and wait (bounded) until
    /// each has exited (see [`close_everywhere`]).
    #[cfg(test)]
    pub(crate) fn close_repo(&self, dir: &Path, timeout: Duration) -> bool {
        close_in(&self.inner, dir, std::time::Instant::now() + timeout)
    }

    /// Idle processes of `dir` (tests).
    #[cfg(test)]
    pub(crate) fn idle_count(&self, dir: &Path) -> usize {
        self.lock().get(dir).map_or(0, |r| r.idle.len())
    }

    /// Repositories with an entry (tests).
    #[cfg(test)]
    pub(crate) fn repo_count(&self) -> usize {
        self.lock().len()
    }

    /// Kill every idle process of `dir` but keep it in the pool (tests:
    /// the next exchange meets a dead process).
    #[cfg(test)]
    pub(crate) async fn kill_idle(&self, dir: &Path) {
        let mut procs = self
            .lock()
            .get_mut(dir)
            .map(|r| std::mem::take(&mut r.idle))
            .unwrap_or_default();
        for i in &mut procs {
            i.proc.kill();
            let _ = i.proc.child.wait().await; // ignore: gone either way
        }
        if let Some(r) = self.lock().get_mut(dir) {
            r.idle.extend(procs);
        }
    }
}

/// Kill `procs` and keep them until they are seen to exit.
fn bury(inner: &PoolInner, dir: &Path, procs: impl IntoIterator<Item = BatchProc>) {
    let mut dying = inner.dying.lock().unwrap_or_else(PoisonError::into_inner);
    for mut proc in procs {
        proc.kill();
        dying.push(Dying {
            dir: dir.to_path_buf(),
            proc,
        });
    }
}

/// Close every batch process of the repository `dir` in every pool of
/// this process — before its files are removed or rewritten (a purge, a
/// gc): on Windows a file another process holds open cannot be deleted.
/// Idle processes are killed; busy ones are killed when their exchange
/// ends (the entry is gone, waiters are refused); then this waits until
/// every one has exited, at most `timeout`. `false`: some were still
/// running at the deadline. Blocking (short sleeps): call it from a
/// blocking context, or through [`close_everywhere_async`].
pub(crate) fn close_everywhere(dir: &Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let pools: Vec<Arc<PoolInner>> = POOLS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    let mut all = true;
    for inner in &pools {
        all &= close_in(inner, dir, deadline);
    }
    all
}

/// [`close_everywhere`] on the blocking pool.
pub(crate) async fn close_everywhere_async(dir: &Path, timeout: Duration) -> bool {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || close_everywhere(&dir, timeout))
        .await
        .unwrap_or(false)
}

fn close_in(inner: &PoolInner, dir: &Path, deadline: std::time::Instant) -> bool {
    let removed = {
        let mut repos = inner.repos.lock().unwrap_or_else(PoisonError::into_inner);
        repos.remove(dir)
    };
    let permits = removed.map(|entry| {
        entry.permits.close();
        bury(inner, dir, entry.idle.into_iter().map(|i| i.proc));
        entry.permits
    });
    loop {
        // Busy leases (and refused waiters) still hold the entry's
        // semaphore; a lease of a closed entry buries its process on drop.
        let busy = permits.as_ref().is_some_and(|p| Arc::strong_count(p) > 1);
        let running = {
            let mut dying = inner.dying.lock().unwrap_or_else(PoisonError::into_inner);
            dying.retain_mut(|d| !d.proc.exited());
            dying.iter().any(|d| d.dir == dir)
        };
        if !busy && !running {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Close processes idle for longer than `idle`, repositories with nothing
/// left, and forget killed processes that have exited.
fn sweep(inner: &PoolInner) {
    let idle = inner.cfg.idle;
    let mut closed = Vec::new();
    {
        let mut repos = inner.repos.lock().unwrap_or_else(PoisonError::into_inner);
        for (dir, r) in repos.iter_mut() {
            let (old, keep): (Vec<_>, Vec<_>) =
                r.idle.drain(..).partition(|i| i.since.elapsed() >= idle);
            r.idle = keep;
            for i in old {
                closed.push((dir.clone(), i.proc)); // clone: the dying list names the repository
            }
        }
        repos.retain(|_, r| {
            !(r.idle.is_empty() && Arc::strong_count(&r.permits) == 1 && r.used.elapsed() >= idle)
        });
    }
    for (dir, proc) in closed {
        bury(inner, &dir, [proc]);
    }
    let mut dying = inner.dying.lock().unwrap_or_else(PoisonError::into_inner);
    dying.retain_mut(|d| !d.proc.exited());
}

/// One process taken from the pool; returned only after a complete
/// exchange (`healthy`) to the entry it came from, otherwise killed.
struct Lease {
    pool: Arc<PoolInner>,
    dir: PathBuf,
    permits: Arc<Semaphore>,
    proc: Option<BatchProc>,
    healthy: bool,
    /// Started for this lease (its first exchange).
    fresh: bool,
    _permit: OwnedSemaphorePermit,
}

impl Lease {
    fn proc(&mut self) -> Result<&mut BatchProc, BatchError> {
        self.proc
            .as_mut()
            .ok_or_else(|| BatchError::Failed(failed("lease_empty")))
    }

    /// The exchange's outcome: a complete answer keeps the process, any
    /// other ends it.
    fn finish<T>(
        &mut self,
        answered: Result<io::Result<T>, tokio::time::error::Elapsed>,
        timeout: Duration,
    ) -> Result<T, BatchError> {
        match answered {
            Ok(Ok(v)) => {
                self.healthy = true;
                Ok(v)
            }
            Ok(Err(e)) => {
                let reason = if e.kind() == io::ErrorKind::InvalidData {
                    "protocol"
                } else {
                    "io"
                };
                Err(BatchError::Failed(failed(reason).with_source(e)))
            }
            Err(_) => Err(BatchError::Failed(
                failed("timeout").with_context("timeout_ms", timeout.as_millis()),
            )),
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let Some(proc) = self.proc.take() else {
            return;
        };
        if self.healthy {
            let mut repos = self
                .pool
                .repos
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(r) = repos
                .get_mut(&self.dir)
                .filter(|r| Arc::ptr_eq(&r.permits, &self.permits))
            {
                r.idle.push(IdleProc {
                    proc,
                    since: Instant::now(),
                });
                return;
            }
        }
        // Failed, or the repository was closed meanwhile.
        bury(&self.pool, &self.dir, [proc]);
    }
}

impl BatchProc {
    async fn info(&mut self, oids: &[&str]) -> io::Result<Vec<Info>> {
        let mut input = Vec::with_capacity(oids.len() * 70 + 6);
        for oid in oids {
            command_line(&mut input, "info", oid)?;
        }
        input.extend_from_slice(b"flush\n");
        let Self { stdin, stdout, .. } = self;
        let write = async {
            stdin.write_all(&input).await?;
            stdin.flush().await
        };
        let read = async {
            let mut answers = Vec::with_capacity(oids.len());
            for oid in oids {
                answers.push(read_info(stdout, oid).await?);
            }
            Ok(answers)
        };
        // Written and read together: git's answers to a long list fill the
        // pipe before the list is written.
        let ((), answers) = tokio::try_join!(write, read)?;
        Ok(answers)
    }

    async fn contents(&mut self, oid: &str, max: u64) -> io::Result<Contents> {
        let found = self.info(&[oid]).await?;
        let (kind, size) = match found.into_iter().next() {
            Some(Info::Found { kind, size }) => (kind, size),
            Some(Info::Missing) => return Ok(Contents::Missing),
            None => return Err(protocol("no_answer")),
        };
        if size > max {
            return Ok(Contents::TooLarge { kind, size });
        }
        let mut input = Vec::with_capacity(80);
        command_line(&mut input, "contents", oid)?;
        input.extend_from_slice(b"flush\n");
        self.stdin.write_all(&input).await?;
        self.stdin.flush().await?;
        // The object changing its type or size between two questions is
        // not git's answer for an immutable object.
        if let Contents::Found { kind: k, data } = read_contents(&mut self.stdout, oid, max).await?
        {
            if k == kind && data.len() as u64 == size {
                return Ok(Contents::Found { kind: k, data });
            }
        }
        Err(protocol("answer_changed"))
    }
}

/// `<command> <oid>\n` for a full hex id only (never anything else on the
/// process's stdin).
fn command_line(input: &mut Vec<u8>, command: &str, oid: &str) -> io::Result<()> {
    let valid = matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit());
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not_an_object_id",
        ));
    }
    input.extend_from_slice(command.as_bytes());
    input.push(b' ');
    input.extend_from_slice(oid.as_bytes());
    input.push(b'\n');
    Ok(())
}

/// One header line without its newline (at most [`MAX_HEADER_BYTES`]).
async fn read_header<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<String> {
    let mut line = Vec::with_capacity(96);
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "output_ended_in_header",
            ));
        }
        match buf.iter().position(|b| *b == b'\n') {
            Some(i) => {
                line.extend_from_slice(buf.get(..i).unwrap_or_default());
                reader.consume(i + 1);
                break;
            }
            None => {
                let n = buf.len();
                line.extend_from_slice(buf);
                reader.consume(n);
            }
        }
        if line.len() > MAX_HEADER_BYTES {
            return Err(protocol("header_too_long"));
        }
    }
    if line.len() > MAX_HEADER_BYTES {
        return Err(protocol("header_too_long"));
    }
    String::from_utf8(line).map_err(|_| protocol("header_not_utf8"))
}

/// `<oid> <type> <size>` or `<oid> missing`, for exactly `oid`.
fn parse_header(line: &str, oid: &str) -> io::Result<Info> {
    let mut fields = line.split(' ');
    let (Some(named), Some(second)) = (fields.next(), fields.next()) else {
        return Err(protocol("header_malformed"));
    };
    if !named.eq_ignore_ascii_case(oid) {
        return Err(protocol("header_names_another_object"));
    }
    match (second, fields.next(), fields.next()) {
        ("missing", None, None) => Ok(Info::Missing),
        (kind @ ("blob" | "tree" | "commit" | "tag"), Some(size), None) => {
            let size = size.parse().map_err(|_| protocol("header_size"))?;
            Ok(Info::Found {
                kind: kind.to_owned(),
                size,
            })
        }
        _ => Err(protocol("header_malformed")),
    }
}

/// The answer to `info <oid>`.
pub(crate) async fn read_info<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    oid: &str,
) -> io::Result<Info> {
    let line = read_header(reader).await?;
    parse_header(&line, oid)
}

/// The answer to `contents <oid>`: the header, `size` bytes and a newline.
/// An object over `max` is a protocol error here (the caller asked
/// `info` first and never sends `contents` for it).
pub(crate) async fn read_contents<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    oid: &str,
    max: u64,
) -> io::Result<Contents> {
    let line = read_header(reader).await?;
    let (kind, size) = match parse_header(&line, oid)? {
        Info::Missing => return Ok(Contents::Missing),
        Info::Found { kind, size } => (kind, size),
    };
    if size > max {
        return Err(protocol("contents_over_cap"));
    }
    let len = usize::try_from(size).map_err(|_| protocol("header_size"))?;
    let mut data = vec![0u8; len];
    reader.read_exact(&mut data).await?;
    let mut end = [0u8; 1];
    reader.read_exact(&mut end).await?;
    if end != *b"\n" {
        return Err(protocol("contents_not_terminated"));
    }
    Ok(Contents::Found { kind, data })
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
