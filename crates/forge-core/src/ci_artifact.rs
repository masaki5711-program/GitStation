//! CI artifacts — the files a job declares under `artifacts: { paths }` of
//! `.forge-ci.yml` (M4 C6, plan 14 §4.1, §9 #6; ADR-0029 decision 5).
//!
//! After a job's script succeeds the runner uploads each regular file under
//! the declared paths (symbolic links are never followed or uploaded), one
//! request per file, with the job's own token. forge stores each file under
//! the storage root, with its size and sha256, for [`RETENTION_DAYS`]
//! (a `keep` row — a deployed version, C11 — is never expired). People and
//! agents read them like the job's log (`CiArtifactRead`: an AI agent only
//! when both marks are open, FG-AUTH-0022); a later job of the same
//! pipeline reads the artifacts of the jobs it `needs`, with its own token.
//!
//! [`check_path`] is the one rule for a stored path, shared by the runner
//! (before it sends) and forge (before it stores).

/// How long an artifact is kept (plan 14 §9 #6).
pub const RETENTION_DAYS: i64 = 30;
/// The largest artifact file, in bytes.
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// The most artifact bytes of one job attempt.
pub const MAX_JOB_BYTES: u64 = 512 * 1024 * 1024;
/// The most artifact files of one job attempt.
pub const MAX_JOB_FILES: i64 = 1000;
/// The longest stored path, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;

/// Why `path` cannot be an artifact's path (`None`: valid): a relative
/// path inside the work tree, `/`-separated, without an empty, `.`, `..` or
/// `.git` component, a backslash, a colon, a control character, or more
/// than [`MAX_PATH_BYTES`].
pub fn check_path(path: &str) -> Option<&'static str> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Some("path_length");
    }
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return Some("path_not_relative");
    }
    if path.chars().any(char::is_control) {
        return Some("path_control_character");
    }
    let bad = path
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == ".." || c.eq_ignore_ascii_case(".git"));
    bad.then_some("path_component")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_artifact_path_stays_inside_the_work_tree() {
        assert_eq!(check_path("target/release/app"), None);
        assert_eq!(check_path("report.html"), None);
        for (bad, reason) in [
            ("", "path_length"),
            ("/etc/passwd", "path_not_relative"),
            ("C:/x", "path_not_relative"),
            ("a\\b", "path_not_relative"),
            ("a/../b", "path_component"),
            ("./a", "path_component"),
            ("a//b", "path_component"),
            (".git/config", "path_component"),
            ("a\nb", "path_control_character"),
        ] {
            assert_eq!(check_path(bad), Some(reason), "{bad:?}");
        }
        assert_eq!(
            check_path(&"a".repeat(MAX_PATH_BYTES + 1)),
            Some("path_length")
        );
    }
}
