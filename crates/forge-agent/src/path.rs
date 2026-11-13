//! Where a part may put a file (ADR-0032 §2): under `.claude/`, or the two
//! root files the builder owns, `CLAUDE.md` and `.mcp.json`. Everything else
//! — `scripts/`, `.git/hooks/`, `../`, absolute paths, drive letters,
//! backslashes — is refused, so a part can only ever write Claude Code's
//! configuration and never a file the build or git itself runs.

/// The root files a part may write besides `.claude/**`.
const ROOT_FILES: [&str; 2] = ["CLAUDE.md", ".mcp.json"];

/// The configuration directory a part writes into.
const CLAUDE_DIR: &str = ".claude/";

/// Per-checkout files Claude Code keeps beside the shared ones; a part never
/// ships them (they are the developer's own).
const RESERVED: [&str; 2] = [".claude/settings.local.json", ".claude/worktrees/"];

/// Longest install path accepted, in bytes.
const MAX_PATH_BYTES: usize = 255;

/// Whether `path` (repository-relative, `/`-separated) is in the area the
/// agent lock manages: `.claude/**`, `CLAUDE.md`, `.mcp.json`. Reserved
/// per-checkout paths are not managed.
pub fn is_managed_path(path: &str) -> bool {
    (ROOT_FILES.contains(&path) || path.starts_with(CLAUDE_DIR))
        && !RESERVED
            .iter()
            .any(|r| path == r.trim_end_matches('/') || path.starts_with(r))
}

/// `Ok` when `path` is an allowed install path; otherwise the reason.
pub fn check_install_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err("path_length");
    }
    if path
        .chars()
        .any(|c| c.is_control() || c == '\\' || c == ':' || c == '\u{feff}')
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
    if !is_managed_path(path) {
        return Err("path_not_allowed");
    }
    // `.claude/` alone names the directory, not a file.
    if path.len() == CLAUDE_DIR.len() && path.starts_with(CLAUDE_DIR) {
        return Err("path_not_allowed");
    }
    Ok(())
}
