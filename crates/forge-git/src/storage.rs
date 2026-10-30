//! Repository storage (ADR-0009): the path of a repository is a pure function
//! of its `RepoId` — `<root>/repos/<shard>/<id>.git`, where `<shard>` is the
//! first two hex digits of sha256(id). URL segments never reach a path.

use std::io;
use std::path::{Path, PathBuf};

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::Git;

/// Repository config key that marks a repository with the provision token of
/// the `repositories` row it was created for (D4). A provisioning retry
/// adopts an existing directory only when this marker matches.
pub const PROVISION_TOKEN_KEY: &str = "forge.provisionToken";

/// The canonical storage root, checked once at startup.
#[derive(Debug, Clone)]
pub struct StorageRoot {
    root: PathBuf,
}

fn rejected(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::StoragePathRejected).with_context("reason", reason)
}

fn io_failed(operation: &'static str, e: io::Error) -> ForgeError {
    ForgeError::new(ErrorCode::StorageIoFailed)
        .with_context("operation", operation)
        .with_source(e)
}

/// Strip the Windows verbatim prefix (`\\?\`) that `canonicalize` adds, so
/// paths compare and print like the configured ones (UNC paths keep theirs).
fn normalize(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

/// Server-side settings of every repository forge creates.
const REPO_SETTINGS: &[(&str, &str)] = &[
    ("receive.fsckObjects", "true"),
    ("transfer.fsckObjects", "true"),
    ("uploadpack.allowFilter", "true"),
    ("receive.advertisePushOptions", "true"),
    // Maintenance runs in `forge worker`, never detached after a push.
    ("receive.autogc", "false"),
    ("gc.auto", "0"),
];

impl StorageRoot {
    /// Open an existing root directory. The root must be a real directory,
    /// not a symlink.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ForgeError> {
        let path = path.as_ref();
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(rejected("root_missing").with_source(e))
            }
            Err(e) => return Err(io_failed("inspect", e)),
        };
        if meta.file_type().is_symlink() {
            return Err(rejected("root_symlink"));
        }
        if !meta.is_dir() {
            return Err(rejected("root_not_dir"));
        }
        let root = std::fs::canonicalize(path)
            .map(normalize)
            .map_err(|e| io_failed("inspect", e))?;
        Ok(Self { root })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    fn shard(id: RepoId) -> String {
        let digest: [u8; 32] = Sha256::digest(id.get().to_string().as_bytes()).into();
        let [first, ..] = digest;
        format!("{first:02x}")
    }

    /// Where the repository lives. Pure: does not touch the file system.
    pub fn path_for(&self, id: RepoId) -> PathBuf {
        self.root
            .join("repos")
            .join(Self::shard(id))
            .join(format!("{}.git", id.get()))
    }

    /// Every directory between the root and `path` (inclusive) must be a real
    /// directory: no symlink, no file.
    fn check_chain(&self, path: &Path, id: RepoId) -> Result<(), ForgeError> {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return Err(rejected("outside_root").with_context("repo_id", id));
        };
        let mut current = self.root.clone(); // clone: walked down component by component
        for component in rel.components() {
            current.push(component);
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(rejected("symlink").with_context("repo_id", id));
                }
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Err(rejected("not_dir").with_context("repo_id", id)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(ForgeError::new(ErrorCode::StorageMissing)
                        .with_context("repo_id", id)
                        .with_context("path", current.display()));
                }
                Err(e) => return Err(io_failed("inspect", e).with_context("repo_id", id)),
            }
        }
        Ok(())
    }

    fn ensure_inside(&self, path: &Path, id: RepoId) -> Result<(), ForgeError> {
        let canonical = std::fs::canonicalize(path)
            .map(normalize)
            .map_err(|e| io_failed("inspect", e).with_context("repo_id", id))?;
        if canonical.starts_with(&self.root) {
            Ok(())
        } else {
            Err(rejected("outside_root").with_context("repo_id", id))
        }
    }

    /// The path of an existing repository, verified to be a real directory
    /// under the root with no symlink on the way.
    pub fn existing(&self, id: RepoId) -> Result<PathBuf, ForgeError> {
        let path = self.path_for(id);
        self.check_chain(&path, id)?;
        self.ensure_inside(&path, id)?;
        Ok(path)
    }

    /// Create the bare repository for `id`, atomically: initialize and
    /// configure it in a temporary sibling directory, mark it with
    /// `forge.provisionToken=<token>` (the repository row's provision token),
    /// then rename it into place. Refuses to touch an existing path
    /// (`FG-REPO-0004`) — a retry uses [`StorageRoot::init_bare_or_adopt`].
    /// `hooks_path` is the forge-owned hooks directory (M1 phase 10 also
    /// forces it per invocation with `-c core.hooksPath`).
    pub async fn init_bare(
        &self,
        git: &Git,
        id: RepoId,
        token: Uuid,
        hooks_path: Option<&Path>,
    ) -> Result<PathBuf, ForgeError> {
        let hooks = hooks_value(hooks_path)?;
        let (path, shard) = self.prepare_shard(id).await?;
        if self.occupant(&path, id).await?.is_some() {
            return Err(occupied(id, &path));
        }
        self.create(git, id, token, hooks.as_deref(), &path, &shard)
            .await
    }

    /// Create `repos/` and the shard one level at a time, verifying each
    /// level before going below it (never follow a planted symlink).
    /// Returns the repository path and its shard directory.
    async fn prepare_shard(&self, id: RepoId) -> Result<(PathBuf, PathBuf), ForgeError> {
        let path = self.path_for(id);
        let shard = path
            .parent()
            .ok_or_else(|| rejected("outside_root").with_context("repo_id", id))?
            .to_path_buf();
        for dir in [self.root.join("repos"), shard.clone()] {
            self.ensure_real_dir(&dir, id).await?;
        }
        self.ensure_inside(&shard, id)?;
        Ok((path, shard))
    }

    /// What is at `path` without following a symlink; `None` when nothing is.
    async fn occupant(
        &self,
        path: &Path,
        id: RepoId,
    ) -> Result<Option<std::fs::Metadata>, ForgeError> {
        match tokio::fs::symlink_metadata(path).await {
            Ok(meta) => Ok(Some(meta)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_failed("inspect", e).with_context("repo_id", id)),
        }
    }

    /// Initialize in a temporary sibling, then rename into `path`.
    async fn create(
        &self,
        git: &Git,
        id: RepoId,
        token: Uuid,
        hooks: Option<&str>,
        path: &Path,
        shard: &Path,
    ) -> Result<PathBuf, ForgeError> {
        // A fresh name per attempt: a temp directory left by a crashed
        // attempt never collides with a retry.
        let tmp = shard.join(format!(".tmp-{}-{}.git", id.get(), Uuid::new_v4().simple()));
        let result = match self.init_into(git, &tmp, token, hooks).await {
            Ok(()) => tokio::fs::rename(&tmp, path).await.map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists || path.exists() {
                    occupied(id, path)
                } else {
                    io_failed("rename", e).with_context("repo_id", id)
                }
            }),
            Err(e) => Err(e),
        };
        if let Err(err) = result {
            return Err(match tokio::fs::remove_dir_all(&tmp).await {
                Ok(()) => err,
                Err(e) if e.kind() == io::ErrorKind::NotFound => err,
                // The original failure stays the reported code; the leftover
                // directory is named so an operator can remove it.
                Err(e) => err
                    .with_context("cleanup_failed", tmp.display())
                    .with_context("cleanup_error", e),
            });
        }
        self.existing(id)
    }

    /// `dir` exists as a real directory afterwards (created if missing); a
    /// symlink or a file there is rejected, never followed.
    async fn ensure_real_dir(&self, dir: &Path, id: RepoId) -> Result<(), ForgeError> {
        for attempt in 0..2 {
            match tokio::fs::symlink_metadata(dir).await {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(rejected("symlink").with_context("repo_id", id));
                }
                Ok(meta) if meta.is_dir() => return Ok(()),
                Ok(_) => return Err(rejected("not_dir").with_context("repo_id", id)),
                Err(e) if e.kind() == io::ErrorKind::NotFound && attempt == 0 => {
                    match tokio::fs::create_dir(dir).await {
                        // Created, or a concurrent creator won: re-check either way.
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => {
                            return Err(io_failed("create_shard", e).with_context("repo_id", id))
                        }
                    }
                }
                Err(e) => return Err(io_failed("inspect", e).with_context("repo_id", id)),
            }
        }
        Err(io_failed(
            "create_shard",
            io::Error::other("directory vanished while being created"),
        )
        .with_context("repo_id", id))
    }

    async fn init_into(
        &self,
        git: &Git,
        dir: &Path,
        token: Uuid,
        hooks: Option<&str>,
    ) -> Result<(), ForgeError> {
        git.command("init")
            .arg("--bare")
            .arg("--quiet")
            .arg("--initial-branch=main")
            // No template: nothing from the system template dir is copied.
            .arg("--template=")
            .path(dir)
            .output()
            .await?;
        let config = dir.join("config");
        configure(git, &config, hooks).await?;
        // Written last: a marked repository is a fully configured one.
        set_config(
            git,
            &config,
            PROVISION_TOKEN_KEY,
            &token.hyphenated().to_string(),
        )
        .await
    }
}

/// Apply the server-side settings (and the hooks path) to the repository
/// config file `config` (absolute). Idempotent: adoption runs it again on an
/// existing repository.
async fn configure(git: &Git, config: &Path, hooks: Option<&str>) -> Result<(), ForgeError> {
    let settings = REPO_SETTINGS
        .iter()
        .copied()
        .chain(hooks.map(|h| ("core.hooksPath", h)));
    for (key, value) in settings {
        set_config(git, config, key, value).await?;
    }
    Ok(())
}

/// `git config --file <config> <key> <value>`: writes exactly that file (no
/// repository discovery, no `GIT_DIR`); callers check it is a regular file.
async fn set_config(
    git: &Git,
    config: &Path,
    key: &'static str,
    value: &str,
) -> Result<(), ForgeError> {
    git.command("config")
        .arg("--file")
        .arg_owned(config)
        .arg(key)
        .arg_owned(value)
        .output()
        .await
        .map(|_| ())
}

/// A lossy or relative hooks path would point git at a directory that does
/// not exist, and the policy hooks would silently never run. Checked before
/// anything is created.
fn hooks_value(hooks_path: Option<&Path>) -> Result<Option<String>, ForgeError> {
    match hooks_path {
        None => Ok(None),
        Some(h) => match h.to_str() {
            Some(s) if h.is_absolute() => Ok(Some(s.to_string())),
            _ => Err(ForgeError::new(ErrorCode::ConfigInvalid)
                .with_context("config_key", "git.hooks_path")
                .with_context("reason", "must_be_absolute_utf8")),
        },
    }
}

/// Whether a provisioning job should retry after `err` from
/// [`StorageRoot::init_bare`] / [`StorageRoot::init_bare_or_adopt`].
///
/// Retryable (transient, nothing left at the final path that a retry could
/// not adopt): git timeout / busy / unavailable, storage I/O failures, and a
/// git failure while *creating* (the temporary directory is removed). Every
/// git failure on an *existing* directory is reported as `FG-REPO-0004`
/// with `adopt_refused`, which — like a rejected path or an invalid hooks
/// path — needs an operator and is not retryable.
pub fn provision_error_is_retryable(err: &ForgeError) -> bool {
    matches!(
        err.code(),
        ErrorCode::GitTimeout
            | ErrorCode::GitBusy
            | ErrorCode::GitUnavailable
            | ErrorCode::GitProcessFailed
            | ErrorCode::StorageIoFailed
    )
}

fn occupied(id: RepoId, path: &Path) -> ForgeError {
    ForgeError::new(ErrorCode::RepoPathOccupied)
        .with_context("repo_id", id)
        .with_context("path", path.display())
}

#[path = "storage_adopt.rs"]
mod adopt;
pub use adopt::Provisioned;

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "storage_adopt_tests.rs"]
mod adopt_tests;

#[cfg(test)]
#[path = "storage_adopt_hardening_tests.rs"]
mod adopt_hardening_tests;
