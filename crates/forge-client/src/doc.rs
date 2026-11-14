//! `gstn doc fmt` / `gstn doc lint` (plan 13 stage 2, ADR-0026 §4–§5):
//! the document format's tools on the working tree. **Local only**: the
//! repository is the one found upward from the current directory (a
//! directory holding `.git`); its files are listed with `git ls-files`
//! (tracked and untracked, `.gitignore` respected — what a commit would
//! hold) and read from disk. Nothing is sent to the server, and no
//! configuration or token is read. (`gstn doc migration`, the server's
//! setting of plan 13 Q13, is [`crate::doc_migration`].)
//!
//! The lint and the format are forge-doc's, the same functions on the same
//! bytes as the server's `forge/doc-lint` (forge-checks), so the answers
//! agree. A document is a `.md` whose front matter names a `doctype`, in any
//! folder; its type is `.forge/doctypes/<type>.yml`.
//!
//! **Bounds** (never a silent pass: `FG-DOC-0002`, exit 1): the listing
//! ([`MAX_LISTING_BYTES`], `doc_tree`), the `.md` files read
//! ([`MAX_MARKDOWN_FILES`], `doc_files`), the bytes read
//! ([`MAX_TOTAL_BYTES`], `doc_bytes`), the type definitions
//! ([`MAX_DOCTYPE_FILES`], `doctype_files`) and a file name git lists that
//! is not UTF-8 (`path_not_utf8`). A document is read up to one byte past
//! the parser's limit (the lint then says `too_large`, as the server does).
//! `git ls-files` has [`GIT_TIMEOUT`].
//!
//! Exit codes: `fmt --check` exits 1 with `FG-DOC-0004` when a file would
//! change; `lint` exits 1 with `FG-DOC-0001` on an error (`FG-DOC-0003`
//! when a type definition cannot be used). Broken references are errors
//! (plan 13 Q13); `--migration-warnings` reports them as warnings.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use forge_doc::doctype::{parse_doctype, Doctype, MAX_DOCTYPE_BYTES};
use forge_doc::document::{parse_document, Problem, MAX_DOCUMENT_BYTES};
use forge_doc::fmt::{format_document, migrate_legacy_with, FmtRefusal, MigrationNote};
use forge_doc::legacy::{is_legacy, SPEC_SYSTEM};
use forge_doc::lint::{lint as lint_documents, FileReport, Source, DOCTYPES_DIR};
use forge_errors::ErrorCode;
use serde_json::{json, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, DocCommand};
use crate::error::{ClientError, Result};

/// `git ls-files` output read at most (`doc_tree` past it).
pub const MAX_LISTING_BYTES: usize = 16 * 1024 * 1024;
/// `.md` files read at most (`doc_files` past it).
pub const MAX_MARKDOWN_FILES: usize = 5_000;
/// Bytes read in all (`doc_bytes` past it).
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Type definitions read at most (`doctype_files` past it).
pub const MAX_DOCTYPE_FILES: usize = 64;
/// The bounds of one read (module docs); the defaults are the constants
/// above (tests use smaller ones).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub listing_bytes: usize,
    pub markdown_files: usize,
    pub total_bytes: u64,
    pub doctype_files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            listing_bytes: MAX_LISTING_BYTES,
            markdown_files: MAX_MARKDOWN_FILES,
            total_bytes: MAX_TOTAL_BYTES,
            doctype_files: MAX_DOCTYPE_FILES,
        }
    }
}

/// Variables that would point git at another repository than the one
/// found (review MR3): removed from `git ls-files`' environment.
const GIT_LOCATION_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// One `git ls-files`.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The rule a migration may report as a warning (plan 13 Q13).
const BROKEN_REFERENCE: &str = "ref_broken";

/// `gstn doc …` (module docs).
/// `gstn doc migration` is the one subcommand that calls the server
/// ([`crate::doc_migration`]); `fmt` and `lint` are local only.
pub fn doc(cli: &Cli, cmd: &DocCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let local = || -> Result<(PathBuf, PathBuf)> {
        let cwd = std::env::current_dir().map_err(|e| {
            ClientError::local(
                ErrorCode::CliInputFailed,
                format!("reason=cwd_unreadable kind={:?}", e.kind()),
            )
        })?;
        let root = find_root(&cwd)?;
        Ok((cwd, root))
    };
    match cmd {
        DocCommand::Migration { command } => crate::doc_migration::migration(cli, command, env, io),
        DocCommand::Show {
            project,
            id,
            rev,
            lang,
            block,
        } => match block {
            None => {
                crate::doc_view::show(cli, *project, id, rev.as_deref(), lang.as_deref(), env, io)
            }
            Some(b) => crate::doc_view::show_block(
                cli,
                *project,
                (id, b),
                rev.as_deref(),
                lang.as_deref(),
                env,
                io,
            ),
        },
        DocCommand::Refs {
            project,
            id,
            block,
            rev,
            limit,
            after,
        } => crate::doc_view::refs(
            cli,
            *project,
            id,
            &crate::doc_view::RefsArgs {
                block: block.as_deref(),
                rev: rev.as_deref(),
                limit: *limit,
                after: after.as_deref(),
            },
            env,
            io,
        ),
        DocCommand::List {
            project,
            rev,
            doctype,
            limit,
            after,
        } => crate::doc_view::list(
            cli,
            *project,
            &crate::doc_view::ListArgs {
                rev: rev.as_deref(),
                doctype: doctype.as_deref(),
                limit: *limit,
                after: after.as_deref(),
            },
            env,
            io,
        ),
        DocCommand::Fmt {
            check,
            migrate,
            accept_status_loss,
            paths,
        } => {
            let (cwd, root) = local()?;
            let targets = targets(&root, &cwd, paths)?;
            let mode = FmtMode {
                check: *check,
                migrate: *migrate,
                accept_status_loss: *accept_status_loss,
            };
            let out = fmt(&root, &targets, mode, &Limits::default())?;
            report_fmt(cli, io, &out)
        }
        DocCommand::Lint {
            paths,
            migration_warnings,
        } => {
            let (cwd, root) = local()?;
            let targets = targets(&root, &cwd, paths)?;
            let out = lint(&root, &targets, *migration_warnings, &Limits::default())?;
            report_lint(cli, io, &out)
        }
    }
}

// ── the repository and its files ───────────────────────────────────────

/// `raw` without the Windows verbatim prefix `canonicalize` adds
/// (`\\?\C:\x` → `C:\x`, `\\?\UNC\srv\share` → `\\srv\share`);
/// `None` when it has none. Git and the user know the plain form.
pub fn strip_verbatim_str(raw: &str) -> Option<String> {
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{rest}"));
    }
    let rest = raw.strip_prefix(r"\\?\")?;
    let mut chars = rest.chars();
    let drive = matches!(
        (chars.next(), chars.next()),
        (Some(c), Some(':')) if c.is_ascii_alphabetic()
    );
    drive.then(|| rest.to_owned())
}

/// `canonicalize`, without the Windows verbatim prefix.
fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let full = path.canonicalize()?;
    Ok(full
        .to_str()
        .and_then(strip_verbatim_str)
        .map_or(full, PathBuf::from))
}

/// The repository's root: the nearest directory from `start` upward that
/// holds `.git` (a directory, or a file for a worktree), canonical.
pub fn find_root(start: &Path) -> Result<PathBuf> {
    let start = canonical(start).map_err(|e| {
        ClientError::local(
            ErrorCode::CliInputFailed,
            format!("reason=cwd_unreadable kind={:?}", e.kind()),
        )
    })?;
    start
        .ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            ClientError::local(
                ErrorCode::CliUsageInvalid,
                "reason=not_in_a_repository (run gstn doc inside a git working tree)",
            )
        })
}

/// The repository-relative prefixes `paths` (relative to `cwd`) name;
/// none: the whole repository (one empty prefix).
pub fn targets(root: &Path, cwd: &Path, paths: &[PathBuf]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(vec![String::new()]);
    }
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let full = canonical(&cwd.join(p)).map_err(|_| {
            ClientError::local(
                ErrorCode::CliUsageInvalid,
                format!("reason=path_not_found path={}", p.display()),
            )
        })?;
        let rel = full.strip_prefix(root).map_err(|_| {
            ClientError::local(
                ErrorCode::CliUsageInvalid,
                format!("reason=path_outside_repository path={}", p.display()),
            )
        })?;
        let mut segments = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(s) => segments.push(s.to_str().ok_or_else(|| {
                    ClientError::local(
                        ErrorCode::CliUsageInvalid,
                        format!("reason=path_not_utf8 path={}", p.display()),
                    )
                })?),
                Component::CurDir => {}
                Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                    return Err(ClientError::local(
                        ErrorCode::CliUsageInvalid,
                        format!("reason=path_outside_repository path={}", p.display()),
                    ))
                }
            }
        }
        out.push(segments.join("/"));
    }
    Ok(out)
}

/// Whether the repository path `path` is one of `targets` or under one.
fn selected(targets: &[String], path: &str) -> bool {
    targets.iter().any(|t| {
        t.is_empty()
            || path
                .strip_prefix(t.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

fn is_doctype_file(path: &str) -> bool {
    path.strip_prefix(DOCTYPES_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| name.ends_with(".yml") && !name.contains('/'))
}

/// The repository's files for the lint or the format.
#[derive(Debug, Default)]
pub struct Files {
    pub doctypes: Vec<(String, Vec<u8>)>,
    pub markdown: Vec<(String, Vec<u8>)>,
    /// Each file's length and time of change when it was read: `fmt`
    /// refuses to write over a file that changed since (review MR3).
    pub stamps: HashMap<String, Stamp>,
    /// The first bound hit (module docs).
    pub incomplete: Option<&'static str>,
}

/// A file's length and time of change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        }
    }
}

impl Files {
    fn left_out(&mut self, reason: &'static str) {
        self.incomplete.get_or_insert(reason);
    }
}

/// `git ls-files -z --cached --others --exclude-standard` in `root`, at
/// most `max` bytes (more: the second value is true). Git is pointed at
/// `root` only: the variables that name another repository are removed.
pub(crate) fn list_files(root: &Path, max: usize) -> Result<(Vec<u8>, bool)> {
    ls_files(root, max, &["--cached", "--others", "--exclude-standard"])
}

/// `git ls-files -z <which>` in `root` (see [`list_files`]); `which` is a
/// fixed set of flags from this crate, never user input.
pub(crate) fn ls_files(root: &Path, max: usize, which: &[&str]) -> Result<(Vec<u8>, bool)> {
    let fail = |why: String| {
        ClientError::local(
            ErrorCode::CliInputFailed,
            format!("reason=git_ls_files_failed ({why})"),
        )
    };
    let mut cmd = Command::new("git");
    for var in GIT_LOCATION_VARS {
        cmd.env_remove(var);
    }
    let mut child = cmd
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .args(which)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| fail(format!("git_not_runnable {:?}", e.kind())))?;
    let Some(stdout) = child.stdout.take() else {
        return Err(fail("no_stdout".to_owned()));
    };
    // Read on a thread so the time limit holds while git writes; dropping
    // the pipe after the cap ends a git that has more to say.
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let limit = u64::try_from(max).unwrap_or(u64::MAX);
        let read = stdout.take(limit.saturating_add(1)).read_to_end(&mut out);
        read.map(|_| out)
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() > GIT_TIMEOUT => {
                let killed = child.kill().and_then(|()| child.wait());
                return Err(fail(format!(
                    "git_timeout cleanup={}",
                    if killed.is_ok() { "ok" } else { "failed" }
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(fail(format!("git_wait_failed {:?}", e.kind()))),
        }
    };
    let out = reader
        .join()
        .map_err(|_| fail("reader_panicked".to_owned()))?
        .map_err(|e| fail(format!("read_failed {:?}", e.kind())))?;
    let cut = out.len() > max;
    // A git that was cut off by the closed pipe fails; that is the cap.
    if !status.success() && !cut {
        return Err(fail(format!("exit {}", status.code().unwrap_or(-1))));
    }
    Ok((out, cut))
}

/// Read up to `max` bytes of `path`; `None` when it is gone or is not a
/// regular file (a symbolic link is not followed: the server reads none).
fn read_prefix(path: &Path, max: usize) -> Result<Option<(Vec<u8>, Stamp)>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        // Listed but deleted in the working tree: not in the next commit.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e.kind())),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let file = std::fs::File::open(path).map_err(|e| unreadable(e.kind()))?;
    let mut out = Vec::new();
    file.take(u64::try_from(max).unwrap_or(u64::MAX))
        .read_to_end(&mut out)
        .map_err(|e| unreadable(e.kind()))?;
    Ok(Some((out, Stamp::of(&meta))))
}

fn unreadable(kind: std::io::ErrorKind) -> ClientError {
    ClientError::local(
        ErrorCode::CliInputFailed,
        format!("reason=file_unreadable kind={kind:?}"),
    )
}

/// Every type definition, and the `.md` files `want` keeps, of the
/// repository at `root` (module docs, bounds).
pub fn read_files(root: &Path, want: impl Fn(&str) -> bool, limits: &Limits) -> Result<Files> {
    let (listing, cut) = list_files(root, limits.listing_bytes)?;
    let mut files = Files::default();
    if cut {
        files.left_out("doc_tree");
    }
    let mut paths: BTreeSet<String> = BTreeSet::new();
    let entries = listing.split(|b| *b == 0).filter(|e| !e.is_empty());
    // The last entry of a cut listing may be cut too.
    let entries: Vec<&[u8]> = if cut {
        let mut all: Vec<&[u8]> = entries.collect();
        all.pop();
        all
    } else {
        entries.collect()
    };
    for raw in entries {
        match std::str::from_utf8(raw) {
            Ok(p) => {
                paths.insert(p.to_owned());
            }
            Err(_) => {
                if raw.ends_with(b".md") || raw.starts_with(DOCTYPES_DIR.as_bytes()) {
                    files.left_out("path_not_utf8");
                }
            }
        }
    }
    let doctypes: Vec<&String> = paths.iter().filter(|p| is_doctype_file(p)).collect();
    if doctypes.len() > limits.doctype_files {
        files.left_out("doctype_files");
    }
    let markdown: Vec<&String> = paths
        .iter()
        .filter(|p| p.ends_with(".md") && want(p))
        .collect();
    if markdown.len() > limits.markdown_files {
        files.left_out("doc_files");
    }
    let mut total = 0u64;
    let chosen = doctypes
        .into_iter()
        .take(limits.doctype_files)
        .map(|p| (p, MAX_DOCTYPE_BYTES.saturating_add(1), true))
        .chain(
            markdown
                .into_iter()
                .take(limits.markdown_files)
                .map(|p| (p, MAX_DOCUMENT_BYTES.saturating_add(1), false)),
        );
    for (path, max, is_type) in chosen {
        let Some((bytes, stamp)) = read_prefix(&root.join(path), max)? else {
            continue;
        };
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if total.saturating_add(size) > limits.total_bytes {
            files.left_out("doc_bytes");
            break;
        }
        total = total.saturating_add(size);
        files.stamps.insert(path.clone(), stamp); // clone: the set keeps its paths
        if is_type {
            files.doctypes.push((path.clone(), bytes)); // clone: the set keeps its paths
        } else {
            files.markdown.push((path.clone(), bytes)); // clone: as above
        }
    }
    Ok(files)
}

// ── lint ───────────────────────────────────────────────────────────────

/// One problem as reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    pub problem: Problem,
    /// `error` or `warning` (a broken reference with --migration-warnings).
    pub level: &'static str,
    pub code: ErrorCode,
}

/// The lint's answer.
#[derive(Debug, Default)]
pub struct LintOutcome {
    /// Per file, by path; only the files the targets select.
    pub files: Vec<(String, Vec<Reported>, usize)>,
    pub errors: usize,
    pub warnings: usize,
    pub markdown_files: usize,
    pub doctype_files: usize,
    pub incomplete: Option<&'static str>,
    pub migration_warnings: bool,
}

impl LintOutcome {
    /// Whether a type definition cannot be used (FG-DOC-0003).
    fn definitions_invalid(&self) -> bool {
        self.files.iter().any(|(_, ps, _)| {
            ps.iter()
                .any(|r| r.code == ErrorCode::DoctypeDefinitionInvalid)
        })
    }
}

/// The code a problem of `path` is reported under (as forge/doc-lint).
fn code_of(path: &str, rule: &str) -> ErrorCode {
    if is_doctype_file(path) || rule == "doctype_invalid" {
        ErrorCode::DoctypeDefinitionInvalid
    } else {
        ErrorCode::DocLintProblem
    }
}

/// Lint the repository at `root`; report the files under `targets`.
pub fn lint(
    root: &Path,
    targets: &[String],
    migration_warnings: bool,
    limits: &Limits,
) -> Result<LintOutcome> {
    // Every document is read: references resolve against the repository.
    let files = read_files(root, |_| true, limits)?;
    let doctypes: Vec<Source<'_>> = files
        .doctypes
        .iter()
        .map(|(path, bytes)| Source { path, bytes })
        .collect();
    let documents: Vec<Source<'_>> = files
        .markdown
        .iter()
        .map(|(path, bytes)| Source { path, bytes })
        .collect();
    let reports: Vec<FileReport> = lint_documents(&doctypes, &documents);
    let mut out = LintOutcome {
        markdown_files: files.markdown.len(),
        doctype_files: files.doctypes.len(),
        incomplete: files.incomplete,
        migration_warnings,
        ..LintOutcome::default()
    };
    for r in reports.into_iter().filter(|r| selected(targets, &r.path)) {
        let reported: Vec<Reported> = r
            .problems
            .into_iter()
            .map(|p| {
                let level = if migration_warnings && p.rule == BROKEN_REFERENCE {
                    "warning"
                } else {
                    "error"
                };
                Reported {
                    code: code_of(&r.path, p.rule),
                    problem: p,
                    level,
                }
            })
            .collect();
        out.warnings += reported.iter().filter(|r| r.level == "warning").count();
        out.errors += reported.iter().filter(|r| r.level == "error").count();
        // Problems past the per-file bound are errors (they are not listed).
        out.errors += r.problems_dropped;
        out.files.push((r.path, reported, r.problems_dropped));
    }
    Ok(out)
}

fn problem_text(path: &str, r: &Reported) -> String {
    let p = &r.problem;
    let subject = p
        .subject
        .as_deref()
        .map(|s| format!(" {}", crate::error::sanitize(s)))
        .unwrap_or_default();
    format!(
        "{path}:{}: {}: {}{subject} [{}]",
        p.line,
        r.level,
        p.rule,
        r.code.id()
    )
}

fn incomplete_json(reason: Option<&'static str>) -> Value {
    reason.map_or(
        Value::Null,
        |r| json!({"error_code": ErrorCode::DocIndexBounded.id(), "reason": r}),
    )
}

pub fn report_lint(cli: &Cli, io: &mut Io<'_>, out: &LintOutcome) -> Result<()> {
    let mut human = Vec::new();
    for (path, problems, dropped) in &out.files {
        human.extend(problems.iter().map(|r| problem_text(path, r)));
        if *dropped > 0 {
            human.push(format!("{path}: {dropped} more problem(s) not listed"));
        }
    }
    human.push(if out.errors == 0 && out.warnings == 0 {
        format!(
            "No problem ({} Markdown file(s) and {} type definition(s) read).",
            out.markdown_files, out.doctype_files
        )
    } else {
        format!("{} error(s), {} warning(s).", out.errors, out.warnings)
    });
    if let Some(reason) = out.incomplete {
        human.push(format!(
            "Only part of the repository was read ({reason}) [{}].",
            ErrorCode::DocIndexBounded.id()
        ));
    }
    let files: Vec<Value> = out
        .files
        .iter()
        .map(|(path, problems, dropped)| {
            json!({
                "path": path,
                "problems": problems.iter().map(|r| json!({
                    "line": r.problem.line,
                    "rule": r.problem.rule,
                    "subject": r.problem.subject,
                    "level": r.level,
                    "error_code": r.code.id(),
                })).collect::<Vec<_>>(),
                "problems_dropped": dropped,
            })
        })
        .collect();
    let value = json!({
        "files": files,
        "errors": out.errors,
        "warnings": out.warnings,
        "markdown_files": out.markdown_files,
        "doctype_files": out.doctype_files,
        "migration_warnings": out.migration_warnings,
        "incomplete": incomplete_json(out.incomplete),
    });
    emit(io, cli, value, &human.join("\n"))?;
    if out.errors > 0 {
        let code = if out.definitions_invalid() {
            ErrorCode::DoctypeDefinitionInvalid
        } else {
            ErrorCode::DocLintProblem
        };
        return Err(ClientError::local(
            code,
            format!("errors={} warnings={}", out.errors, out.warnings),
        ));
    }
    if let Some(reason) = out.incomplete {
        return Err(ClientError::local(
            ErrorCode::DocIndexBounded,
            format!("reason={reason}"),
        ));
    }
    Ok(())
}

// ── fmt ────────────────────────────────────────────────────────────────

/// The format's answer.
#[derive(Debug, Default)]
pub struct FmtOutcome {
    pub check: bool,
    /// Formatted (or, with --check, that would be), by path.
    pub changed: Vec<String>,
    pub unchanged: usize,
    /// Left as they are, with why (by path).
    pub skipped: Vec<(String, &'static str)>,
    pub definitions_invalid: bool,
    pub incomplete: Option<&'static str>,
    /// `--migrate`: the older-format files rewritten (or, with --check,
    /// that would be), with what a person must still fill in.
    pub migrated: Vec<(String, Vec<MigrationNote>)>,
}

/// How `fmt` runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FmtMode {
    /// Write nothing.
    pub check: bool,
    /// Also migrate older-format specifications (ADR-0026 §7).
    pub migrate: bool,
    /// `--accept-status-loss` ([`forge_doc::fmt::migrate_legacy_with`]).
    pub accept_status_loss: bool,
}

/// Write `bytes` over `path` through a new temporary file in the same
/// directory and a rename (never half a file), keeping its permissions.
/// `read` is the file as it was read: a file that changed since (its length
/// or time of change) is left as it is (review MR3: never overwrite an edit
/// made while formatting).
pub fn replace_file(path: &Path, bytes: &[u8], read: Option<&Stamp>) -> Result<()> {
    use std::io::Write as _;
    let fail = |reason: &str, kind: std::io::ErrorKind| {
        ClientError::local(
            ErrorCode::CliOutputFailed,
            format!("stream=file reason={reason} kind={kind:?}"),
        )
    };
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| fail("write_failed", std::io::ErrorKind::InvalidInput))?;
    let tmp = path.with_file_name(format!(".{name}.gstn-fmt-{}.tmp", std::process::id()));
    let meta = std::fs::metadata(path).map_err(|e| fail("write_failed", e.kind()))?;
    // `create_new`: never write through a file (or link) already there.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| fail("write_failed", e.kind()))?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::set_permissions(&tmp, meta.permissions()));
    drop(file);
    let unchanged = std::fs::metadata(path)
        .map(|now| read.is_none_or(|r| Stamp::of(&now) == *r))
        .unwrap_or(false);
    let result = match written {
        Err(e) => Err(fail("write_failed", e.kind())),
        Ok(()) if !unchanged => Err(fail(
            "file_changed_while_formatting",
            std::io::ErrorKind::Other,
        )),
        Ok(()) => std::fs::rename(&tmp, path).map_err(|e| fail("write_failed", e.kind())),
    };
    if result.is_err() {
        // The temporary file is ours; the failure is what is reported.
        let _cleanup = std::fs::remove_file(&tmp);
    }
    result
}

/// Whether `bytes` (a file fmt refused) names a `doctype` in its front
/// matter, read as far as the parser's limit and lossily: a too large or
/// not UTF-8 document is a document fmt could not check; any other
/// Markdown file is none of its concern.
fn names_doctype(bytes: &[u8]) -> bool {
    let head = bytes.get(..MAX_DOCUMENT_BYTES).unwrap_or(bytes);
    let text = String::from_utf8_lossy(head);
    parse_document(text.as_bytes()).doctype().is_some()
}

/// Format the documents under `targets` (all: the whole repository);
/// `mode.check`: write nothing; `mode.migrate`: an older-format
/// specification (`spec:`, no `doctype:`) is migrated first
/// ([`migrate_legacy`]), with the repository's `spec.system` type when it
/// defines one. Without `--migrate` such a file is never touched.
pub fn fmt(root: &Path, targets: &[String], mode: FmtMode, limits: &Limits) -> Result<FmtOutcome> {
    let check = mode.check;
    let files = read_files(root, |p| selected(targets, p), limits)?;
    let mut types: HashMap<String, Option<Doctype>> = HashMap::new();
    for (path, bytes) in &files.doctypes {
        let stem = path
            .rsplit('/')
            .next()
            .and_then(|n| n.strip_suffix(".yml"))
            .unwrap_or_default();
        types.insert(stem.to_owned(), parse_doctype(stem, bytes).ok());
    }
    let mut out = FmtOutcome {
        check,
        incomplete: files.incomplete,
        ..FmtOutcome::default()
    };
    for (path, bytes) in &files.markdown {
        let named = targets.iter().any(|t| t == path);
        let parsed = parse_document(bytes);
        if mode.migrate && is_legacy(&parsed) {
            migrate_one(
                root,
                path,
                bytes,
                &types,
                &files,
                mode.accept_status_loss,
                &mut out,
            )?;
            continue;
        }
        let doctype_name = parsed.doctype().map(str::to_owned);
        let ty = match doctype_name.as_deref().map(|n| types.get(n)) {
            // A definition that cannot be used: its order is unknown.
            Some(Some(None)) => {
                out.definitions_invalid = true;
                out.skipped.push((path.clone(), "doctype_invalid")); // clone: the answer owns its paths
                continue;
            }
            Some(Some(Some(t))) => Some(t),
            // No definition (the lint says `doctype_unknown`), or no type.
            Some(None) | None => None,
        };
        match format_document(bytes, ty) {
            Ok(formatted) if formatted.as_bytes() == bytes.as_slice() => out.unchanged += 1,
            Ok(formatted) => {
                if !check {
                    replace_file(
                        &root.join(path),
                        formatted.as_bytes(),
                        files.stamps.get(path),
                    )?;
                }
                out.changed.push(path.clone()); // clone: as above
            }
            // Not a document: none of fmt's concern, unless it was named.
            Err(FmtRefusal::NotADocument) if !named => {}
            Err(FmtRefusal::TooLarge | FmtRefusal::NotUtf8) if !named && !names_doctype(bytes) => {}
            Err(why) => out.skipped.push((path.clone(), why.as_str())), // clone: as above
        }
    }
    Ok(out)
}

/// Migrate (then format) one older-format file into `out`.
fn migrate_one(
    root: &Path,
    path: &String,
    bytes: &[u8],
    types: &HashMap<String, Option<Doctype>>,
    files: &Files,
    accept_status_loss: bool,
    out: &mut FmtOutcome,
) -> Result<()> {
    let ty = match types.get(SPEC_SYSTEM) {
        Some(None) => {
            out.definitions_invalid = true;
            out.skipped.push((path.clone(), "doctype_invalid")); // clone: the answer owns its paths
            return Ok(());
        }
        Some(Some(t)) => Some(t),
        None => None,
    };
    match migrate_legacy_with(bytes, ty, accept_status_loss) {
        Ok(m) => {
            if !out.check {
                replace_file(&root.join(path), m.text.as_bytes(), files.stamps.get(path))?;
            }
            out.changed.push(path.clone()); // clone: as above
            out.migrated.push((path.clone(), m.notes)); // clone: as above
        }
        // An older file this cannot read safely: named, never skipped
        // silently (the person asked for the migration).
        Err(why) => out.skipped.push((path.clone(), why.as_str())), // clone: as above
    }
    Ok(())
}

pub fn report_fmt(cli: &Cli, io: &mut Io<'_>, out: &FmtOutcome) -> Result<()> {
    let verb = if out.check {
        "would change"
    } else {
        "formatted"
    };
    let notes_of = |p: &String| {
        out.migrated
            .iter()
            .find(|(m, _)| m == p)
            .map(|(_, notes)| notes)
    };
    let mut human: Vec<String> = out
        .changed
        .iter()
        .map(|p| match notes_of(p) {
            Some(notes) => {
                let todo: Vec<&str> = notes.iter().map(|n| n.as_str()).collect();
                format!("{verb}: {p} (migrated; {})", todo.join(", "))
            }
            None => format!("{verb}: {p}"),
        })
        .collect();
    human.extend(
        out.skipped
            .iter()
            .map(|(p, why)| format!("skipped: {p} ({why})")),
    );
    human.push(format!(
        "{} file(s) {verb}, {} already formatted, {} skipped.",
        out.changed.len(),
        out.unchanged,
        out.skipped.len()
    ));
    if !out.migrated.is_empty() {
        human.push(
            "Migrated files: fill in each TODO(gstn doc fmt --migrate) (gstn doc lint names \
             field_missing owner)."
                .to_owned(),
        );
    }
    if let Some(reason) = out.incomplete {
        human.push(format!(
            "Only part of the repository was read ({reason}) [{}].",
            ErrorCode::DocIndexBounded.id()
        ));
    }
    let value = json!({
        "check": out.check,
        "migrated": out.migrated.iter().map(|(p, notes)| json!({
            "path": p,
            "notes": notes.iter().map(|n| n.as_str()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "changed": out.changed,
        "unchanged": out.unchanged,
        "skipped": out.skipped.iter().map(|(p, why)| json!({"path": p, "reason": why})).collect::<Vec<_>>(),
        "incomplete": incomplete_json(out.incomplete),
    });
    emit(io, cli, value, &human.join("\n"))?;
    // A file fmt could not check is never a pass of --check (review MR3).
    let unchecked = out
        .skipped
        .iter()
        .filter(|(_, why)| *why != "doctype_invalid")
        .count();
    if out.check && (!out.changed.is_empty() || unchecked > 0) {
        return Err(ClientError::local(
            ErrorCode::DocFormatWouldChange,
            format!("files={} unchecked={unchecked}", out.changed.len()),
        ));
    }
    if out.definitions_invalid {
        return Err(ClientError::local(
            ErrorCode::DoctypeDefinitionInvalid,
            "reason=doctype_invalid (gstn doc lint names the definition)",
        ));
    }
    if let Some(reason) = out.incomplete {
        return Err(ClientError::local(
            ErrorCode::DocIndexBounded,
            format!("reason={reason}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "doc_tests.rs"]
mod tests;
