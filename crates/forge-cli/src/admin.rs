//! `forge admin …` — operator commands that inspect, never mutate.

use std::path::Path;

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::StorageRoot;

/// `forge admin repo-path <repo_id>`: where a repository lives on disk
/// (ADR-0009 — the path is derived from the id only) and whether it exists.
///
/// Only a genuinely missing directory prints `missing`; a rejected path
/// (symlink, not a directory — possible tampering) or an I/O failure is
/// returned as an error, so the CLI boundary logs it with its code.
pub fn repo_path(storage_root: &Path, repo_id: i64) -> Result<String, ForgeError> {
    let root = StorageRoot::open(storage_root)?;
    let id = RepoId::new(repo_id);
    let path = root.path_for(id);
    let state = match root.existing(id) {
        Ok(_) => "exists",
        Err(e) if e.code() == ErrorCode::StorageMissing => "missing",
        Err(e) => return Err(e),
    };
    Ok(format!("{}\t{state}\n", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_path_prints_id_derived_path_and_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = repo_path(dir.path(), 42).expect("repo-path");
        assert!(out.contains("42.git"), "{out}");
        assert!(out.trim_end().ends_with("missing"), "{out}");
    }

    #[test]
    fn repo_path_reports_exists_for_a_repository_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = StorageRoot::open(dir.path()).expect("root");
        std::fs::create_dir_all(root.path_for(RepoId::new(42))).expect("dir");
        let out = repo_path(dir.path(), 42).expect("repo-path");
        assert!(out.trim_end().ends_with("exists"), "{out}");
    }

    #[test]
    fn repo_path_does_not_report_rejected_storage_as_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = StorageRoot::open(dir.path()).expect("root");
        let path = root.path_for(RepoId::new(42));
        std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
        std::fs::write(&path, b"not a repository").expect("write");
        let err = repo_path(dir.path(), 42).expect_err("rejected, not missing");
        assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    }

    #[test]
    fn repo_path_rejects_a_root_that_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").expect("write");
        let err = repo_path(&file, 1).expect_err("bad root");
        assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    }
}
