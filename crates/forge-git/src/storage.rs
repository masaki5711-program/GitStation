//! Repository storage (ADR-0009): the path of a repository is a pure function
//! of its `RepoId` — `<root>/repos/<shard>/<id>.git`, where `<shard>` is the
//! first two hex digits of sha256(id). URL segments never reach a path.

use std::path::{Path, PathBuf};

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use sha2::{Digest, Sha256};

use crate::Git;

/// The canonical storage root, checked once at startup.
#[derive(Debug, Clone)]
pub struct StorageRoot {
    root: PathBuf,
}

fn rejected(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::StoragePathRejected).with_context("reason", reason)
}

/// Strip the Windows verbatim prefix (`\\?\`) that `canonicalize` adds, so
/// paths compare and print like the configured ones.
fn normalize(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

impl StorageRoot {
    /// Open an existing root directory. The root must be a real directory,
    /// not a symlink.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ForgeError> {
        let path = path.as_ref();
        let meta =
            std::fs::symlink_metadata(path).map_err(|e| rejected("root_not_dir").with_source(e))?;
        if meta.file_type().is_symlink() {
            return Err(rejected("root_symlink"));
        }
        if !meta.is_dir() {
            return Err(rejected("root_not_dir"));
        }
        let root = std::fs::canonicalize(path)
            .map(normalize)
            .map_err(|e| rejected("root_not_dir").with_source(e))?;
        Ok(Self { root })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    fn shard(id: RepoId) -> String {
        let digest = Sha256::digest(id.get().to_string().as_bytes());
        format!("{:02x}", digest.first().copied().unwrap_or(0))
    }

    /// Where the repository lives. Pure: does not touch the file system.
    pub fn path_for(&self, id: RepoId) -> PathBuf {
        self.root
            .join("repos")
            .join(Self::shard(id))
            .join(format!("{}.git", id.get()))
    }

    /// The path of an existing repository, verified to be a real directory
    /// under the root with no symlink on the way.
    pub fn existing(&self, id: RepoId) -> Result<PathBuf, ForgeError> {
        let path = self.path_for(id);
        for p in [path.parent().map(Path::to_path_buf), Some(path.clone())]
            .into_iter()
            .flatten()
        {
            match std::fs::symlink_metadata(&p) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(rejected("symlink").with_context("repo_id", id));
                }
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Err(rejected("not_dir").with_context("repo_id", id)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ForgeError::new(ErrorCode::StorageMissing)
                        .with_context("repo_id", id)
                        .with_context("path", p.display()));
                }
                Err(e) => {
                    return Err(rejected("unreadable")
                        .with_context("repo_id", id)
                        .with_source(e))
                }
            }
        }
        self.ensure_inside(&path, id)?;
        Ok(path)
    }

    fn ensure_inside(&self, path: &Path, id: RepoId) -> Result<(), ForgeError> {
        let canonical = std::fs::canonicalize(path).map(normalize).map_err(|e| {
            rejected("unreadable")
                .with_context("repo_id", id)
                .with_source(e)
        })?;
        if canonical.starts_with(&self.root) {
            Ok(())
        } else {
            Err(rejected("outside_root").with_context("repo_id", id))
        }
    }

    /// Create a bare repository for `id` with forge's server-side settings.
    /// `hooks_path` is the forge-owned hooks directory (set from M1 phase 10).
    pub async fn init_bare(
        &self,
        git: &Git,
        id: RepoId,
        hooks_path: Option<&Path>,
    ) -> Result<PathBuf, ForgeError> {
        let path = self.path_for(id);
        if let Some(shard) = path.parent() {
            std::fs::create_dir_all(shard).map_err(|e| {
                rejected("create_failed")
                    .with_context("repo_id", id)
                    .with_source(e)
            })?;
        }
        git.command("init")
            .arg("--bare")
            .arg("--quiet")
            .arg("--initial-branch=main")
            .operand(&path)
            .output()
            .await?;
        let mut settings: Vec<(&str, String)> = vec![
            ("receive.fsckObjects", "true".into()),
            ("transfer.fsckObjects", "true".into()),
            ("uploadpack.allowFilter", "true".into()),
            ("receive.advertisePushOptions", "true".into()),
        ];
        if let Some(hooks) = hooks_path {
            settings.push(("core.hooksPath", hooks.display().to_string()));
        }
        for (key, value) in settings {
            git.command("config")
                .current_dir(&path)
                .arg(key)
                .arg(value)
                .output()
                .await?;
        }
        self.existing(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GitConfig;

    fn root() -> (tempfile::TempDir, StorageRoot) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = StorageRoot::open(dir.path()).expect("open root");
        (dir, root)
    }

    #[test]
    fn path_for_never_escapes_root() {
        let (_d, root) = root();
        for raw in [0, 1, -1, 42, i64::MAX, i64::MIN] {
            let p = root.path_for(RepoId::new(raw));
            assert!(p.starts_with(root.path()), "{}", p.display());
            assert!(!p.to_string_lossy().contains(".."));
        }
        assert_ne!(
            root.path_for(RepoId::new(1)).parent(),
            root.path_for(RepoId::new(2)).parent(),
            "shards spread repositories"
        );
    }

    #[test]
    fn missing_repository_dir_is_storage_missing() {
        let (_d, root) = root();
        let err = root.existing(RepoId::new(7)).expect_err("missing");
        assert_eq!(err.code(), ErrorCode::StorageMissing);
    }

    #[test]
    fn root_that_is_a_file_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").expect("write");
        let err = StorageRoot::open(&file).expect_err("file root");
        assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    }

    #[cfg(unix)]
    #[test]
    fn path_outside_root_is_rejected() {
        let (_d, root) = root();
        let outside = tempfile::tempdir().expect("tempdir");
        let path = root.path_for(RepoId::new(9));
        let shard = path.parent().expect("shard");
        std::fs::create_dir_all(shard).expect("shard");
        std::os::unix::fs::symlink(outside.path(), &path).expect("symlink");
        let err = root.existing(RepoId::new(9)).expect_err("symlink escape");
        assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    }

    /// Windows: creating symlinks needs a privilege the test runner may lack,
    /// so this variant checks the other rejection on the same path: a
    /// repository entry that is not a directory. Linux CI runs the symlink case.
    #[cfg(windows)]
    #[test]
    fn path_outside_root_is_rejected() {
        let (_d, root) = root();
        let path = root.path_for(RepoId::new(9));
        std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
        std::fs::write(&path, b"not a repository").expect("write");
        let err = root.existing(RepoId::new(9)).expect_err("not a dir");
        assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    }

    #[tokio::test]
    async fn init_bare_creates_a_bare_repository_with_server_settings() {
        let (_d, root) = root();
        let home = tempfile::tempdir().expect("home");
        let git = Git::new(GitConfig::new("git", home.path()));
        let path = root
            .init_bare(&git, RepoId::new(5), None)
            .await
            .expect("init");
        assert!(path.join("HEAD").is_file());
        let out = git
            .command("config")
            .current_dir(&path)
            .arg("--get")
            .arg("receive.fsckObjects")
            .output()
            .await
            .expect("config");
        assert_eq!(out.stdout_str().trim(), "true");
        let bare = git
            .command("rev-parse")
            .current_dir(&path)
            .arg("--is-bare-repository")
            .output()
            .await
            .expect("rev-parse");
        assert_eq!(bare.stdout_str().trim(), "true");
    }
}
