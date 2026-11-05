//! The push checks' scan (M3 X8, ADR-0008 amendment X8): what a push
//! brings, read inside git's quarantine by the pre-receive hook, checked
//! for blobs over the size limit and for secrets ([`crate::secret_scan`]).
//!
//! **The size check is complete or it refuses** (X8 review: a scan that
//! stopped at a cap must not let a large blob through). It streams every
//! object git received for this push — the quarantine's own object
//! directory, without the repository's objects as alternates:
//!
//! ```text
//! GIT_OBJECT_DIRECTORY=<quarantine> git cat-file --batch-all-objects --unordered \
//!     --batch-check='%(objectname) %(objecttype) %(objectsize)'
//! ```
//!
//! read line by line (no output held in memory; only the blobs over the
//! limit are kept). It has its own bound ([`ScanLimits::size_timeout`]);
//! any failure or timeout is an error (FG-GIT-0023, the push is refused).
//! A blob only an annotated tag or a tree names is sized like any other.
//! Without a quarantine git received no pack (git ≥ 2.38, which forge
//! requires, always quarantines a received pack): nothing new to size.
//!
//! **The secret scan may stop early** (fail open with a warn). It looks at
//! every object reachable from the new tips and from no ref before the
//! push:
//!
//! ```text
//! git rev-list --objects --stdin --not --all        (tips on stdin)
//! git cat-file --batch-check='%(objectname) %(objecttype) %(objectsize)'
//! git cat-file --batch                               (text blobs ≤ 1 MiB)
//! git log --reverse --raw --no-abbrev --no-renames --format=%H --stdin --not --all
//! ```
//!
//! The last one runs only when something was found, to name the commit
//! that adds each finding's blob (the oldest such commit; a blob reachable
//! only through a merge or a tag has none). Inside the hook the refs are
//! still the ones before the push, so `--not --all` excludes exactly the
//! history the repository already has: an old secret never refuses a new
//! push. The quarantine variables git gave the hook
//! (`GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`) are
//! checked against the repository ([`Quarantine::from_hook_env`]) and
//! passed to these commands only; `GIT_QUARANTINE_PATH` is set too, so git
//! itself refuses any ref update from them.
//!
//! Bounds of the secret scan ([`ScanLimits`]): one deadline, an object
//! count cap and a byte cap of content read. Hitting one, or a text blob
//! too large to scan (over 1 MiB, under the size limit), makes the report
//! incomplete (`objects_cap`, `bytes_cap`, `deadline`,
//! `large_blob_unscanned`) — the caller lets the push through with a warn
//! (FG-GIT-0024). Binary blobs (a NUL in the first 8000 bytes) are counted
//! (`skipped_binary`) but do not make it incomplete: the rules are text
//! patterns. A git failure other than a timeout is an error (FG-GIT-0023,
//! fail closed). Object ids reach git only on stdin, validated.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::time::Instant;

use crate::command::{Git, GitCommand};
use crate::secret_scan::{self, SecretScan};
use crate::upload_request::is_object_id;

/// The bound of the secret scan.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(60);
/// The bound of the size check (complete, or the push is refused).
pub const SIZE_TIMEOUT: Duration = Duration::from_secs(120);
/// The most new objects looked at.
pub const MAX_OBJECTS: usize = 500_000;
/// Blobs larger than this are not scanned for secrets (only sized).
pub const SECRET_BLOB_MAX: u64 = 1024 * 1024;
/// The most blob content read for the secret scan.
pub const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;
/// The findings kept (all are counted).
pub const MAX_FINDINGS: usize = 20;
/// Content read per `cat-file --batch` call.
const BATCH_BYTES: u64 = 8 * 1024 * 1024;
/// The longest path kept for a finding (bytes, at a character boundary).
const MAX_PATH: usize = 256;

/// The scan's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    /// A blob larger than this refuses the push (`git.http.push_max_blob_bytes`).
    pub max_blob_bytes: u64,
    pub secret_blob_max: u64,
    pub max_scan_bytes: u64,
    pub max_objects: usize,
    /// The secret scan's bound (past it: incomplete, accepted).
    pub timeout: Duration,
    /// The size check's bound (past it: refused).
    pub size_timeout: Duration,
    pub max_findings: usize,
}

impl ScanLimits {
    /// The fixed bounds with the configured blob size limit.
    pub fn new(max_blob_bytes: u64) -> Self {
        Self {
            max_blob_bytes,
            secret_blob_max: SECRET_BLOB_MAX,
            max_scan_bytes: MAX_SCAN_BYTES,
            max_objects: MAX_OBJECTS,
            timeout: SCAN_TIMEOUT,
            size_timeout: SIZE_TIMEOUT,
            max_findings: MAX_FINDINGS,
        }
    }
}

/// What tripped a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingKind {
    /// A blob over `max_blob_bytes` (its size in bytes).
    LargeBlob { size: u64 },
    /// A secret rule ([`secret_scan`]); never the value.
    Secret { rule: String },
}

impl FindingKind {
    /// The rule name (`large_blob` for the size check).
    pub fn rule(&self) -> &str {
        match self {
            Self::LargeBlob { .. } => "large_blob",
            Self::Secret { rule } => rule,
        }
    }

    pub fn code(&self) -> ErrorCode {
        match self {
            Self::LargeBlob { .. } => ErrorCode::PushBlobTooLarge,
            Self::Secret { .. } => ErrorCode::PushSecretDetected,
        }
    }
}

/// One finding: which blob, at which path, added by which commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: FindingKind,
    pub blob: String,
    /// The path rev-list named it by (control characters replaced by `?`,
    /// at most 256 bytes); empty for a blob no tree names.
    pub path: String,
    /// The oldest new commit whose diff adds the blob.
    pub commit: Option<String>,
}

/// What the scan saw.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanReport {
    /// At most `max_findings`, in rev-list order.
    pub findings: Vec<Finding>,
    /// Every finding counted.
    pub total_findings: usize,
    /// New objects looked at.
    pub objects: usize,
    /// Text blobs scanned for secrets, and their bytes.
    pub blobs_scanned: usize,
    pub bytes_scanned: u64,
    /// Blobs whose finding the allow marker waived (all counted; at most
    /// `max_findings` kept in `waivers`).
    pub allowed: usize,
    pub waivers: Vec<Waiver>,
    /// Objects git received for the push (the size check's count).
    pub pack_objects: usize,
    /// Text blobs over `secret_blob_max` (not scanned for secrets) and
    /// binary blobs (not scanned).
    pub skipped_large: usize,
    pub skipped_binary: usize,
    /// Why the secret scan stopped early or left something out
    /// (`objects_cap`, `bytes_cap`, `deadline`, `large_blob_unscanned`).
    pub incomplete: Option<&'static str>,
}

/// A finding the allow marker waived (serve logs each with its path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiver {
    pub rule: String,
    pub blob: String,
    pub path: String,
}

/// git's quarantine of the push being received, checked against the
/// repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quarantine {
    objects: PathBuf,
    alternates: OsString,
}

fn quarantine_invalid(detail: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::PushCheckFailed)
        .with_context("reason", "quarantine_invalid")
        .with_context("detail", detail)
}

fn canonical(path: &Path, detail: &'static str) -> Result<PathBuf, ForgeError> {
    std::fs::canonicalize(path).map_err(|e| quarantine_invalid(detail).with_source(e))
}

impl Quarantine {
    /// From the variables git gives a pre-receive hook. `None` when git did
    /// not quarantine (nothing was received, e.g. a delete-only push): the
    /// objects are then the repository's own. The object directory must be
    /// `<repo>/objects/tmp_objdir-incoming-*` and every alternate the
    /// repository's `objects`.
    pub fn from_hook_env(
        repo: &Path,
        object_dir: Option<&OsStr>,
        alternates: Option<&OsStr>,
    ) -> Result<Option<Self>, ForgeError> {
        let Some(object_dir) = object_dir.filter(|v| !v.is_empty()) else {
            return match alternates.filter(|v| !v.is_empty()) {
                None => Ok(None),
                Some(_) => Err(quarantine_invalid("alternates_without_object_dir")),
            };
        };
        let Some(alternates) = alternates.filter(|v| !v.is_empty()) else {
            return Err(quarantine_invalid("object_dir_without_alternates"));
        };
        let objects = canonical(&repo.join("objects"), "repo_objects")?;
        let dir = canonical(Path::new(object_dir), "object_dir")?;
        let named = dir
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|n| n.starts_with("tmp_objdir-incoming-"));
        if !named || dir.parent() != Some(objects.as_path()) {
            return Err(quarantine_invalid("object_dir_outside_repository"));
        }
        let Some(list) = alternates.to_str() else {
            return Err(quarantine_invalid("alternates_not_utf8"));
        };
        let sep = if cfg!(windows) { ';' } else { ':' };
        for entry in list.split(sep) {
            if entry.starts_with('"') || canonical(Path::new(entry), "alternate")? != objects {
                return Err(quarantine_invalid("alternate_outside_repository"));
            }
        }
        Ok(Some(Self {
            objects: PathBuf::from(object_dir),
            alternates: alternates.to_os_string(),
        }))
    }
}

/// One listed object.
struct Listed {
    id: String,
    path: Vec<u8>,
}

impl Git {
    /// Scan what a push brings (see the module docs). `tips`: the new
    /// values of the push's updates (zeros left out by the caller).
    pub async fn scan_push(
        &self,
        repo: &Path,
        quarantine: Option<&Quarantine>,
        tips: &[String],
        limits: &ScanLimits,
    ) -> Result<ScanReport, ForgeError> {
        if !tips.iter().all(|t| is_object_id(t)) {
            return Err(ForgeError::new(ErrorCode::PushCheckFailed)
                .with_context("reason", "stdin_invalid")
                .with_context("detail", "tip_not_an_object_id"));
        }
        let mut report = ScanReport::default();
        if tips.is_empty() {
            return Ok(report);
        }
        // 1. Sizes: every received object, complete or refused.
        let (large, total_large) = self
            .size_check(repo, quarantine, limits, &mut report)
            .await?;
        // 2. Secrets: the new objects with their names (may stop early).
        let scan = Scan {
            git: self,
            repo,
            quarantine,
            deadline: Instant::now() + limits.timeout,
        };
        let listed = scan
            .list(tips, limits, &mut report)
            .await?
            .unwrap_or_default();
        report.objects = listed.len();
        let mut found: Vec<(usize, FindingKind)> = Vec::new();
        if !listed.is_empty() {
            match scan.sizes(&listed).await? {
                None => {
                    report.incomplete.get_or_insert("deadline");
                }
                Some(sized) => {
                    let mut candidates: Vec<(usize, u64)> = Vec::new();
                    let mut planned = 0u64;
                    for (i, size) in sized {
                        if size > limits.max_blob_bytes {
                            // A size finding already (step 1).
                        } else if size > limits.secret_blob_max {
                            report.skipped_large += 1;
                            report.incomplete.get_or_insert("large_blob_unscanned");
                        } else if planned.saturating_add(size) > limits.max_scan_bytes {
                            report.incomplete.get_or_insert("bytes_cap");
                        } else {
                            planned += size;
                            candidates.push((i, size));
                        }
                    }
                    scan.secrets(&listed, &candidates, &mut found, limits, &mut report)
                        .await?;
                }
            }
        }
        found.sort_by_key(|(i, _)| *i);
        let path_of = |id: &str| {
            listed
                .iter()
                .find(|l| l.id == id)
                .map(|l| clean_path(&l.path))
                .unwrap_or_default()
        };
        let mut findings: Vec<Finding> = large
            .into_iter()
            .map(|(blob, size)| Finding {
                kind: FindingKind::LargeBlob { size },
                path: path_of(&blob),
                blob,
                commit: None,
            })
            .collect();
        report.total_findings = total_large + found.len();
        for (i, kind) in found {
            if findings.len() >= limits.max_findings {
                break;
            }
            if let Some(l) = listed.get(i) {
                findings.push(Finding {
                    kind,
                    blob: l.id.clone(), // clone: the report owns its copy
                    path: clean_path(&l.path),
                    commit: None,
                });
            }
        }
        if !findings.is_empty() {
            scan.attribute(tips, &mut findings).await;
        }
        report.findings = findings;
        Ok(report)
    }
}

impl Git {
    /// Every object git received for the push, streamed: the blobs over
    /// the limit (at most `max_findings` kept, all counted). Complete or an
    /// error — never partial (see the module docs).
    async fn size_check(
        &self,
        repo: &Path,
        quarantine: Option<&Quarantine>,
        limits: &ScanLimits,
        report: &mut ScanReport,
    ) -> Result<(Vec<(String, u64)>, usize), ForgeError> {
        let Some(q) = quarantine else {
            return Ok((Vec::new(), 0));
        };
        let size_failed = |detail: &'static str| {
            ForgeError::new(ErrorCode::PushCheckFailed)
                .with_context("reason", "size_check_failed")
                .with_context("detail", detail)
        };
        let mut stream = self
            .command("cat-file")
            .repo(repo)
            .env("GIT_OBJECT_DIRECTORY", q.objects.clone().into_os_string())
            .env("GIT_QUARANTINE_PATH", q.objects.clone().into_os_string())
            .arg("--batch-all-objects")
            .arg("--unordered")
            .arg("--batch-check=%(objectname) %(objecttype) %(objectsize)")
            .timeout(limits.size_timeout)
            .idle_timeout(limits.size_timeout)
            .spawn_stream("push-check")
            .await
            .map_err(|e| {
                size_failed("spawn")
                    .with_context("cause_code", e.code().id())
                    .with_source(e)
            })?;
        drop(stream.take_stdin());
        let Some(stdout) = stream.take_stdout() else {
            return Err(size_failed("stdout_missing"));
        };
        let mut lines = BufReader::new(stdout);
        let mut line = Vec::new();
        let mut large = Vec::new();
        let mut total = 0usize;
        let mut read_error = None;
        let mut unreadable = false;
        loop {
            line.clear();
            match lines.read_until(b'\n', &mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    read_error = Some(e);
                    break;
                }
            }
            let parsed = std::str::from_utf8(&line).ok().and_then(|text| {
                let mut parts = text.trim_end().split(' ');
                let (id, kind, size) = (parts.next()?, parts.next()?, parts.next()?);
                let size: u64 = size.parse().ok()?;
                is_object_id(id).then_some((id, kind, size))
            });
            let Some((id, kind, size)) = parsed else {
                unreadable = true;
                break;
            };
            report.pack_objects += 1;
            if kind == "blob" && size > limits.max_blob_bytes {
                total += 1;
                if large.len() < limits.max_findings {
                    large.push((id.to_owned(), size));
                }
            }
        }
        drop(lines);
        // A timeout (FG-GIT-0002) or a failed git refuses the push here.
        stream.finish().await.map_err(|e| {
            size_failed("git")
                .with_context("cause_code", e.code().id())
                .with_source(e)
        })?;
        if let Some(e) = read_error {
            return Err(size_failed("read").with_source(e));
        }
        if unreadable {
            return Err(size_failed("unreadable"));
        }
        Ok((large, total))
    }
}

/// One scan's shared state.
struct Scan<'a> {
    git: &'a Git,
    repo: &'a Path,
    quarantine: Option<&'a Quarantine>,
    deadline: Instant,
}

impl Scan<'_> {
    /// `None`: the deadline passed.
    fn command(&self, sub: &'static str) -> Option<GitCommand> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left < Duration::from_millis(1) {
            return None;
        }
        let mut cmd = self.git.command(sub).repo(self.repo).timeout(left);
        if let Some(q) = self.quarantine {
            cmd = cmd
                .env("GIT_OBJECT_DIRECTORY", q.objects.clone().into_os_string())
                .env("GIT_QUARANTINE_PATH", q.objects.clone().into_os_string())
                .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", q.alternates.clone());
        }
        Some(cmd)
    }

    /// The new objects with their names; `None` when the deadline passed
    /// before any was listed (the report says so).
    async fn list(
        &self,
        tips: &[String],
        limits: &ScanLimits,
        report: &mut ScanReport,
    ) -> Result<Option<Vec<Listed>>, ForgeError> {
        let Some(cmd) = self.command("rev-list") else {
            report.incomplete = Some("deadline");
            return Ok(None);
        };
        let stdin = tips.iter().map(|t| format!("{t}\n")).collect::<String>();
        let cap = self.git.config().max_output_bytes;
        let run = cmd
            .arg("--objects")
            .arg("--stdin")
            .arg("--not")
            .arg("--all")
            .stdin(stdin.into_bytes())
            .output_truncated(cap)
            .await;
        let (out, truncated) = match run {
            Ok(r) => r,
            Err(e) if e.code() == ErrorCode::GitTimeout => {
                report.incomplete = Some("deadline");
                return Ok(None);
            }
            Err(e) => return Err(git_failed("rev-list", e)),
        };
        let mut stdout = out.stdout;
        if truncated {
            // The last line may be cut: keep whole lines only.
            let keep = stdout
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(0, |p| p + 1);
            stdout.truncate(keep);
            report.incomplete = Some("objects_cap");
        }
        let mut listed: Vec<Listed> = Vec::new();
        for line in stdout.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let (head, path) = match line.iter().position(|b| *b == b' ') {
                Some(at) => (line.get(..at).unwrap_or_default(), line.get(at + 1..)),
                None => (line, None),
            };
            match std::str::from_utf8(head).ok().filter(|h| is_object_id(h)) {
                Some(id) => listed.push(Listed {
                    id: id.to_owned(),
                    path: path.unwrap_or_default().to_vec(),
                }),
                // A path with a newline: the rest of the previous name.
                None => {
                    if let Some(last) = listed.last_mut() {
                        last.path.push(b'\n');
                        last.path.extend_from_slice(line);
                    }
                }
            }
        }
        if listed.len() > limits.max_objects {
            listed.truncate(limits.max_objects);
            report.incomplete = Some("objects_cap");
        }
        Ok(Some(listed))
    }

    /// `(index, size)` of every blob; `None`: the deadline passed.
    async fn sizes(&self, listed: &[Listed]) -> Result<Option<Vec<(usize, u64)>>, ForgeError> {
        let Some(cmd) = self.command("cat-file") else {
            return Ok(None);
        };
        let stdin = listed
            .iter()
            .map(|l| format!("{}\n", l.id))
            .collect::<String>();
        let run = cmd
            .arg("--batch-check=%(objectname) %(objecttype) %(objectsize)")
            .stdin(stdin.into_bytes())
            .output()
            .await;
        let out = match run {
            Ok(out) => out,
            Err(e) if e.code() == ErrorCode::GitTimeout => return Ok(None),
            Err(e) => return Err(git_failed("cat-file", e)),
        };
        let text = out.stdout_str();
        let mut sizes = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let mut parts = line.split(' ');
            let (Some(_), Some(kind), Some(size)) = (parts.next(), parts.next(), parts.next())
            else {
                continue; // `<id> missing`: nothing to check
            };
            if kind != "blob" {
                continue;
            }
            let Ok(size) = size.parse::<u64>() else {
                return Err(ForgeError::new(ErrorCode::PushCheckFailed)
                    .with_context("reason", "git_failed")
                    .with_context("detail", "batch_check_unreadable"));
            };
            sizes.push((i, size));
        }
        Ok(Some(sizes))
    }

    /// Read `candidates` in batches and scan each.
    async fn secrets(
        &self,
        listed: &[Listed],
        candidates: &[(usize, u64)],
        found: &mut Vec<(usize, FindingKind)>,
        limits: &ScanLimits,
        report: &mut ScanReport,
    ) -> Result<(), ForgeError> {
        let mut rest = candidates;
        while !rest.is_empty() {
            let take = batch_len(rest);
            let (batch, after) = rest.split_at(take);
            rest = after;
            let Some(cmd) = self.command("cat-file") else {
                report.incomplete.get_or_insert("deadline");
                return Ok(());
            };
            let stdin = batch
                .iter()
                .filter_map(|(i, _)| listed.get(*i))
                .map(|l| format!("{}\n", l.id))
                .collect::<String>();
            let run = cmd.arg("--batch").stdin(stdin.into_bytes()).output().await;
            let out = match run {
                Ok(out) => out,
                Err(e) if e.code() == ErrorCode::GitTimeout => {
                    report.incomplete.get_or_insert("deadline");
                    return Ok(());
                }
                Err(e) => return Err(git_failed("cat-file", e)),
            };
            let blobs = split_batch(&out.stdout)?;
            if blobs.len() != batch.len() {
                return Err(ForgeError::new(ErrorCode::PushCheckFailed)
                    .with_context("reason", "git_failed")
                    .with_context("detail", "batch_count_mismatch"));
            }
            for ((i, _), content) in batch.iter().zip(blobs) {
                report.blobs_scanned += 1;
                report.bytes_scanned += content.len() as u64;
                match secret_scan::scan(content) {
                    SecretScan::Found(rule) => found.push((
                        *i,
                        FindingKind::Secret {
                            rule: rule.to_owned(),
                        },
                    )),
                    SecretScan::Allowed(rule) => {
                        report.allowed += 1;
                        if report.waivers.len() < limits.max_findings {
                            if let Some(l) = listed.get(*i) {
                                report.waivers.push(Waiver {
                                    rule: rule.to_owned(),
                                    blob: l.id.clone(), // clone: the report owns its copy
                                    path: clean_path(&l.path),
                                });
                            }
                        }
                    }
                    SecretScan::Binary => report.skipped_binary += 1,
                    SecretScan::Clean => {}
                }
            }
        }
        Ok(())
    }

    /// Name the commit that adds each finding's blob. Best effort: a
    /// failure leaves `commit` empty (the finding still refuses).
    async fn attribute(&self, tips: &[String], findings: &mut [Finding]) {
        let Some(cmd) = self.command("log") else {
            return;
        };
        let stdin = tips.iter().map(|t| format!("{t}\n")).collect::<String>();
        let cap = self.git.config().max_output_bytes;
        let Ok((out, _)) = cmd
            .arg("--reverse")
            .arg("--raw")
            .arg("--no-abbrev")
            .arg("--no-renames")
            .arg("--format=%H")
            .arg("--stdin")
            .arg("--not")
            .arg("--all")
            .stdin(stdin.into_bytes())
            .output_truncated(cap)
            .await
        else {
            return;
        };
        let text = out.stdout_str();
        let mut commit: Option<&str> = None;
        for line in text.lines() {
            if is_object_id(line) {
                commit = Some(line);
                continue;
            }
            // `:<mode> <mode> <old> <new> <status>\t<path>`
            let Some(raw) = line.strip_prefix(':') else {
                continue;
            };
            let (head, path) = raw.split_once('\t').unwrap_or((raw, ""));
            let Some(new) = head.split(' ').nth(3) else {
                continue;
            };
            for f in findings.iter_mut() {
                if f.commit.is_none() && f.blob == new {
                    f.commit = commit.map(str::to_owned);
                    if f.path.is_empty() {
                        f.path = clean_path(path.as_bytes());
                    }
                }
            }
        }
    }
}

/// How many of `rest` one `cat-file --batch` call reads: at least one,
/// then while the content stays within [`BATCH_BYTES`].
fn batch_len(rest: &[(usize, u64)]) -> usize {
    let mut bytes = 0u64;
    let mut n = 0;
    for (_, size) in rest {
        if n > 0 && bytes.saturating_add(*size) > BATCH_BYTES {
            break;
        }
        bytes = bytes.saturating_add(*size);
        n += 1;
    }
    n.max(1).min(rest.len())
}

/// `cat-file --batch` output: `<id> <type> <size>\n<content>\n` per object.
fn split_batch(out: &[u8]) -> Result<Vec<&[u8]>, ForgeError> {
    let unreadable = || {
        ForgeError::new(ErrorCode::PushCheckFailed)
            .with_context("reason", "git_failed")
            .with_context("detail", "batch_unreadable")
    };
    let mut blobs = Vec::new();
    let mut rest = out;
    while !rest.is_empty() {
        let nl = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(unreadable)?;
        let header =
            std::str::from_utf8(rest.get(..nl).unwrap_or_default()).map_err(|_| unreadable())?;
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(unreadable)?;
        let start = nl + 1;
        let content = rest.get(start..start + size).ok_or_else(unreadable)?;
        blobs.push(content);
        rest = rest.get(start + size + 1..).unwrap_or_default();
    }
    Ok(blobs)
}

fn git_failed(subcommand: &'static str, e: ForgeError) -> ForgeError {
    ForgeError::new(ErrorCode::PushCheckFailed)
        .with_context("reason", "git_failed")
        .with_context("subcommand", subcommand)
        .with_context("cause_code", e.code().id())
        .with_source(e)
}

/// A path as the pusher may see it: control characters replaced, cut.
pub(crate) fn clean_path(raw: &[u8]) -> String {
    let text: String = String::from_utf8_lossy(raw)
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    let mut cut = text.len().min(MAX_PATH);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.get(..cut).unwrap_or_default().to_owned()
}

#[cfg(test)]
#[path = "push_scan_tests.rs"]
pub(crate) mod tests;
