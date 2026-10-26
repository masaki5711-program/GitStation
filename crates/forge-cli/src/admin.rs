//! `forge admin …` — operator commands that inspect, never mutate.

use std::path::Path;

use forge_core::RepoId;
use forge_errors::ForgeError;
use forge_git::StorageRoot;

/// `forge admin repo-path <repo_id>`: where a repository lives on disk
/// (ADR-0009 — the path is derived from the id only).
pub fn repo_path(storage_root: &Path, repo_id: i64) -> Result<String, ForgeError> {
    let root = StorageRoot::open(storage_root)?;
    let path = root.path_for(RepoId::new(repo_id));
    let state = match root.existing(RepoId::new(repo_id)) {
        Ok(_) => "exists",
        Err(_) => "missing",
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
}
