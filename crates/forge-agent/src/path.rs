//! Where a part may put a file (ADR-0032 §2): under `.claude/`, or the two
//! root files the builder owns, `CLAUDE.md` and `.mcp.json`. Everything else
//! — `scripts/`, `.git/hooks/`, `../`, absolute paths, drive letters,
//! backslashes — is refused, so a part can only ever write Claude Code's
//! configuration and never a file the build or git itself runs.
//!
//! **Case and Unicode** (security review A1, finding 1): Windows and macOS
//! open `.Claude/settings.json`, `CLAUDE.MD` or `.MCP.json` as the real
//! configuration, so the managed area is matched ignoring ASCII case. An
//! install path must be ASCII (no Unicode normalisation can make two names
//! one file) and use the canonical spelling; two paths that differ only by
//! case are one file ([`case_key`]).

/// The root files a part may write besides `.claude/**` (canonical case).
const ROOT_FILES: [&str; 2] = ["CLAUDE.md", ".mcp.json"];

/// The configuration directory a part writes into (canonical case).
const CLAUDE_DIR: &str = ".claude/";

/// Per-checkout files Claude Code keeps beside the shared ones: a part never
/// ships them, and the lock ignores them only while they are untracked
/// (finding 2: Claude Code loads hooks from `settings.local.json` too).
const PER_CHECKOUT: [&str; 2] = [".claude/settings.local.json", ".claude/worktrees/"];

/// Longest install path accepted, in bytes.
const MAX_PATH_BYTES: usize = 255;

/// The key two paths share when one file system may treat them as the same
/// file: ASCII-lower-cased (non-ASCII paths are refused as install paths).
pub fn case_key(path: &str) -> String {
    path.to_ascii_lowercase()
}

/// Whether `path` (repository-relative, `/`-separated) is in the area the
/// agent lock manages — `.claude` itself, `.claude/**`, `CLAUDE.md`,
/// `.mcp.json` — in any letter case.
pub fn is_managed_path(path: &str) -> bool {
    let key = case_key(path);
    key == ".claude" || key.starts_with(CLAUDE_DIR) || ROOT_FILES.iter().any(|r| key == case_key(r))
}

/// Whether `path` is a per-checkout file (in any letter case): skipped by
/// the lock only when git does not track it.
pub fn is_per_checkout_path(path: &str) -> bool {
    let key = case_key(path);
    PER_CHECKOUT
        .iter()
        .any(|r| key == r.trim_end_matches('/') || key.starts_with(r))
}

/// `Ok` when `path` is an allowed install path; otherwise the reason.
pub fn check_install_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err("path_length");
    }
    if path
        .chars()
        .any(|c| !c.is_ascii() || c.is_ascii_control() || c == '\\' || c == ':')
    {
        return Err("path_characters");
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err("path_not_allowed");
        }
        // Windows drops a trailing dot or space, so `a.` would land on `a`.
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err("path_characters");
        }
        if segment.eq_ignore_ascii_case(".git") {
            return Err("path_not_allowed");
        }
    }
    // The canonical spelling only: `.Claude/x` is managed but not installable.
    if !(ROOT_FILES.contains(&path) || path.starts_with(CLAUDE_DIR)) {
        return Err("path_not_allowed");
    }
    if is_per_checkout_path(path) {
        return Err("path_not_allowed");
    }
    Ok(())
}
