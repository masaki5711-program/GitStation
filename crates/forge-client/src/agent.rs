//! `gstn agent verify` (plan 16 §2.2, §6, ADR-0032 §7): checks a
//! repository's Claude Code configuration on the developer's PC — not by a
//! hook in the repository, which the configuration itself could change.
//! **Local only** in A1: nothing is sent to the server, no token is read.
//!
//! - Without `--part`: the repository found upward from the current
//!   directory. Its managed files (`.claude/**`, `CLAUDE.md`, `.mcp.json`;
//!   listed by `git ls-files --cached --others --exclude-standard`, read
//!   from disk — what Claude Code would run) are checked against
//!   `.gitstation/agent.lock.json` (or `--lock`), and every part's digest
//!   and Ed25519 signature with the public keys in `agent-keys.json` in the
//!   gstn configuration directory (or `--keys`). The output says the keys
//!   are local and that revocation was not checked (the server's key and
//!   revocation list come with A5).
//! - With `--part <dir>`: one part directory — `part.toml`, install paths,
//!   the static scan (with `--mcp-allowed-hosts`) — and its digest.
//!
//! Line endings: a checkout with `core.autocrlf=true` turns LF into CRLF,
//! so every file is `file_changed` — the check fails closed (FG-AGENT-0007
//! says how to fix the checkout).
//!
//! Exit 1 with the most serious problem's code (FG-AGENT-0002 before 0003,
//! 0005, 0013, 0007, 0001); every problem is listed. Bounds (never a silent
//! pass): [`MAX_TREE_FILES`] managed files and [`MAX_TREE_BYTES`] bytes,
//! the lock and the key list [`MAX_SMALL_FILE`] bytes each — past them
//! `FG-CLI-0009` (CliInputFailed) with the reason.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use forge_agent::{
    is_managed_path, is_per_checkout_path, parse_part, scan, verify_lock, EntryKind, KeyRing, Lock,
    Problem, ScanPolicy, TreeEntry, LOCK_PATH, MAX_PART_BYTES, MAX_PART_FILES,
};
use forge_errors::ErrorCode;
use serde_json::{json, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{AgentCommand, Cli};
use crate::error::{ClientError, Result};

/// The key list's file name in the gstn configuration directory.
pub const KEYS_FILE: &str = "agent-keys.json";
/// Most managed files read from a working tree.
pub const MAX_TREE_FILES: usize = 2_000;
/// Most bytes of managed files read from a working tree.
pub const MAX_TREE_BYTES: usize = 32 * 1024 * 1024;
/// Most bytes of the lock or the key list.
pub const MAX_SMALL_FILE: usize = 4 * 1024 * 1024;
/// Deepest directory level read under `--part`.
const MAX_DEPTH: usize = 32;

/// `gstn agent …` (module docs).
pub fn agent(cli: &Cli, cmd: &AgentCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        AgentCommand::Verify {
            part: Some(dir),
            mcp_allowed_hosts,
            ..
        } => verify_part_dir(cli, io, dir, mcp_allowed_hosts.as_deref().unwrap_or(&[])),
        AgentCommand::Verify {
            part: None,
            lock,
            keys,
            ..
        } => verify_repo(cli, env, io, lock.as_deref(), keys.as_deref()),
    }
}

fn input(reason: &str) -> ClientError {
    ClientError::local(ErrorCode::CliInputFailed, format!("reason={reason}"))
}

/// Up to `max` bytes of `path` (`None`: it does not exist; more than `max`:
/// `too_large`).
fn read_small(path: &Path, max: usize, what: &str) -> Result<Option<Vec<u8>>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(input(&format!("{what}_unreadable kind={:?}", e.kind()))),
    };
    let mut out = Vec::new();
    file.take(u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|e| input(&format!("{what}_unreadable kind={:?}", e.kind())))?;
    if out.len() > max {
        return Err(input(&format!("{what}_too_large")));
    }
    Ok(Some(out))
}

/// Managed path → bytes.
type Tree = BTreeMap<String, Vec<u8>>;

/// The managed paths `git ls-files <which>` lists (non-UTF-8 names kept,
/// lossily, so they are reported rather than skipped).
fn listed(root: &Path, which: &[&str]) -> Result<Vec<String>> {
    let (listing, cut) = crate::doc::ls_files(root, crate::doc::MAX_LISTING_BYTES, which)?;
    if cut {
        return Err(input("agent_tree_listing_too_large"));
    }
    Ok(listing
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .filter(|p| is_managed_path(p))
        .collect())
}

/// The repository's managed files: path → bytes, plus the managed paths
/// that are not regular files (a symlink is never followed).
///
/// Security review A1: tracked files are always managed — a tracked
/// `.claude/settings.local.json` is checked like any other (finding 2);
/// untracked per-checkout files are the developer's own and skipped. A
/// `.claude` that is not a directory (a symlink, or the text file git
/// writes for one where symlinks are off) is irregular, never "no
/// configuration" (finding 3).
fn managed_tree(root: &Path) -> Result<(Tree, Vec<String>)> {
    let mut paths = listed(root, &["--cached"])?;
    paths.extend(
        listed(root, &["--others", "--exclude-standard"])?
            .into_iter()
            .filter(|p| !is_per_checkout_path(p)),
    );
    let mut tree = BTreeMap::new();
    let mut irregular = Vec::new();
    match std::fs::symlink_metadata(root.join(".claude")) {
        Ok(m) if !m.is_dir() => irregular.push(".claude".to_owned()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(input(&format!("agent_file_unreadable kind={:?}", e.kind()))),
    }
    let mut total = 0usize;
    for path in paths {
        if tree.contains_key(&path) || irregular.contains(&path) {
            continue;
        }
        if tree.len() >= MAX_TREE_FILES {
            return Err(input("agent_tree_too_many_files"));
        }
        if path.contains(char::REPLACEMENT_CHARACTER) || path.eq_ignore_ascii_case(".claude") {
            irregular.push(path);
            continue;
        }
        let full = root.join(&path);
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(m) => m,
            // Listed but deleted in the working tree: Claude Code cannot run it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(input(&format!("agent_file_unreadable kind={:?}", e.kind()))),
        };
        if !meta.is_file() {
            irregular.push(path);
            continue;
        }
        let left = MAX_TREE_BYTES.saturating_sub(total);
        let Some(bytes) =
            read_small(&full, left, "agent_file").map_err(|_| input("agent_tree_too_large"))?
        else {
            continue;
        };
        total += bytes.len();
        tree.insert(path, bytes);
    }
    Ok((tree, irregular))
}

fn keys_problem(reason: &'static str) -> Problem {
    Problem {
        code: ErrorCode::SigningKeyUnavailable,
        reason,
        subject: None,
    }
}

/// The key list: `--keys`, or `agent-keys.json` in the configuration
/// directory.
fn load_keys(env: EnvFn<'_>, over: Option<&Path>) -> std::result::Result<KeyRing, Problem> {
    let path = match over {
        Some(p) => p.to_path_buf(),
        None => crate::config::config_dir(env, None)
            .map_err(|_| keys_problem("keys_missing"))?
            .join(KEYS_FILE),
    };
    match read_small(&path, MAX_SMALL_FILE, "keys") {
        Ok(Some(bytes)) => KeyRing::parse(&bytes),
        Ok(None) => Err(keys_problem("keys_missing")),
        Err(_) => Err(keys_problem("keys_unreadable")),
    }
}

fn verify_repo(
    cli: &Cli,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
    lock_over: Option<&Path>,
    keys_over: Option<&Path>,
) -> Result<()> {
    let cwd = std::env::current_dir().map_err(|_| input("cwd_unreadable"))?;
    let root = crate::doc::find_root(&cwd)?;
    let (lock_path, shown) = match lock_over {
        Some(p) => (p.to_path_buf(), p.display().to_string()),
        None => (root.join(LOCK_PATH), LOCK_PATH.to_owned()),
    };
    let (tree, irregular) = managed_tree(&root)?;
    let mut problems: Vec<Problem> = irregular
        .iter()
        .map(|p| Problem {
            code: ErrorCode::AgentLockMismatch,
            reason: "not_a_regular_file",
            subject: Some(p.clone()),
        })
        .collect();
    let lock_bytes = match read_small(&lock_path, MAX_SMALL_FILE, "lock") {
        Ok(b) => b,
        Err(_) => {
            problems.push(Problem {
                code: ErrorCode::AgentLockMismatch,
                reason: "lock_unreadable",
                subject: Some(shown.clone()),
            });
            None
        }
    };
    let (mut parts, mut files) = (0, 0);
    match lock_bytes.as_deref().map(Lock::parse) {
        None if tree.is_empty() && problems.is_empty() => {
            let value = json!({
                "lock": Value::Null, "parts": 0, "files": 0, "keys": "local",
                "revocation_checked": false, "problems": [],
            });
            return emit(
                io,
                cli,
                value,
                "No agent configuration here (no managed files, no agent.lock.json).",
            );
        }
        None => {
            if lock_bytes.is_none() && !problems.iter().any(|p| p.reason == "lock_unreadable") {
                problems.push(Problem {
                    code: ErrorCode::AgentLockMismatch,
                    reason: "lock_missing",
                    subject: Some(shown.clone()),
                });
            }
        }
        Some(Err(p)) => problems.push(p),
        Some(Ok(lock)) => {
            let keys = if lock.parts.is_empty() {
                Ok(KeyRing::default())
            } else {
                load_keys(env, keys_over)
            };
            let ring_ = match keys {
                Ok(k) => k,
                Err(p) => {
                    problems.push(p);
                    KeyRing::default()
                }
            };
            let keys_failed = problems
                .iter()
                .any(|p| p.code == ErrorCode::SigningKeyUnavailable);
            let report = verify_lock(&lock, &ring_, &tree);
            parts = report.parts;
            files = report.files;
            // Without a key list every part is `unknown_key`: the one
            // `keys_missing` line says it once.
            problems.extend(
                report
                    .problems
                    .into_iter()
                    .filter(|p| !(keys_failed && p.reason == "unknown_key")),
            );
        }
    }
    let summary = format!(
        "Checked {parts} part(s) and {files} file(s) of {shown} with local keys; revocation not checked (offline)."
    );
    let value = json!({
        "lock": shown,
        "parts": parts,
        "files": files,
        "keys": "local",
        "revocation_checked": false,
        "problems": problems.iter().map(problem_json).collect::<Vec<_>>(),
    });
    report(cli, io, value, &problems, &summary)
}

/// The part directory's entries, paths relative to `dir` with `/`.
fn read_part_dir(dir: &Path) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut total = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = vec![(dir.to_path_buf(), 0)];
    while let Some((at, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err(input("part_too_deep"));
        }
        let listing = std::fs::read_dir(&at)
            .map_err(|e| input(&format!("part_unreadable kind={:?}", e.kind())))?;
        for item in listing {
            let item = item.map_err(|e| input(&format!("part_unreadable kind={:?}", e.kind())))?;
            let path = item.path();
            let rel = relative(dir, &path)?;
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|e| input(&format!("part_unreadable kind={:?}", e.kind())))?;
            // One past the limit is enough for forge-agent to say so.
            if entries.len() > MAX_PART_FILES {
                return Ok(entries);
            }
            if meta.is_dir() {
                stack.push((path, depth + 1));
            } else if meta.is_file() {
                let left = (MAX_PART_BYTES + 1).saturating_sub(total);
                let content = read_small(&path, left, "part_file")
                    .map_err(|_| input("part_too_large"))?
                    .unwrap_or_default();
                total += content.len();
                entries.push(TreeEntry::file(&rel, content));
            } else {
                let kind = if meta.file_type().is_symlink() {
                    EntryKind::Symlink
                } else {
                    EntryKind::Other
                };
                entries.push(TreeEntry {
                    path: rel,
                    kind,
                    content: Vec::new(),
                });
            }
        }
    }
    Ok(entries)
}

fn relative(dir: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(dir)
        .map_err(|_| input("part_path_outside"))?;
    let mut segments = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => {
                segments.push(s.to_str().ok_or_else(|| input("part_path_not_utf8"))?)
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return Err(input("part_path_outside")),
        }
    }
    Ok(segments.join("/"))
}

fn verify_part_dir(cli: &Cli, io: &mut Io<'_>, dir: &Path, allowed: &[String]) -> Result<()> {
    let entries = read_part_dir(dir)?;
    let shown = dir.display().to_string();
    let policy = ScanPolicy {
        // clone: the policy owns its hosts
        mcp_allowed_hosts: allowed.to_vec(),
    };
    let (part, problems) = match parse_part(&entries) {
        Ok(part) => {
            let found = scan(&part, &policy);
            (Some(part), found)
        }
        Err(problems) => (None, problems),
    };
    let value = json!({
        "part": shown,
        "slug": part.as_ref().map(|p| p.manifest.slug.clone()),
        "kind": part.as_ref().map(|p| p.manifest.kind.as_str()),
        "version": part.as_ref().map(|p| p.manifest.version.clone()),
        "capabilities": part.as_ref().map(|p| {
            p.manifest.capabilities.iter().map(|c| c.as_str()).collect::<Vec<_>>()
        }),
        "files": part.as_ref().map_or(0, |p| p.files.len()),
        "digest": part.as_ref().map(|p| p.digest.to_string()),
        "problems": problems.iter().map(problem_json).collect::<Vec<_>>(),
    });
    let summary = match &part {
        Some(p) => format!(
            "{} {} {} ({} file(s), capabilities: {}) digest {}",
            p.manifest.kind.as_str(),
            p.manifest.slug,
            p.manifest.version,
            p.files.len(),
            if p.manifest.capabilities.is_empty() {
                "none".to_owned()
            } else {
                p.manifest
                    .capabilities
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            p.digest
        ),
        None => format!("{shown}: not a valid part."),
    };
    report(cli, io, value, &problems, &summary)
}

fn problem_json(p: &Problem) -> Value {
    json!({"error_code": p.code.id(), "reason": p.reason, "subject": p.subject})
}

fn problem_text(p: &Problem) -> String {
    let subject = p
        .subject
        .as_deref()
        .map(|s| format!("{}: ", crate::error::sanitize(s)))
        .unwrap_or_default();
    format!("{subject}{} [{}]", p.reason, p.code.id())
}

fn report(
    cli: &Cli,
    io: &mut Io<'_>,
    value: Value,
    problems: &[Problem],
    summary: &str,
) -> Result<()> {
    let mut human: Vec<String> = problems.iter().map(problem_text).collect();
    human.push(if problems.is_empty() {
        summary.to_owned()
    } else {
        format!("{} problem(s). {summary}", problems.len())
    });
    emit(io, cli, value, &human.join("\n"))?;
    match forge_agent::worst(problems) {
        None => Ok(()),
        Some(p) => Err(ClientError::local(
            p.code,
            format!(
                "reason={} subject={} problems={}",
                p.reason,
                p.subject.as_deref().unwrap_or("-"),
                problems.len()
            ),
        )),
    }
}
