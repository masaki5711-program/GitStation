//! Provisioning retry (D4): adopt a repository an earlier attempt of the same
//! provisioning job created, identified by its `forge.provisionToken` marker.
//! Anything else at the path stays `FG-REPO-0004`, so a restored database
//! never adopts a directory that is not its own.

use std::path::{Path, PathBuf};

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use uuid::Uuid;

use super::{configure, hooks_value, io_failed, occupied, StorageRoot, PROVISION_TOKEN_KEY};
use crate::Git;

/// Result of [`StorageRoot::init_bare_or_adopt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provisioned {
    /// Nothing was at the path; the repository was created now.
    Created(PathBuf),
    /// An earlier attempt with the same token created it; it was adopted and
    /// its configuration re-applied.
    Adopted(PathBuf),
}

impl Provisioned {
    pub fn path(&self) -> &Path {
        match self {
            Self::Created(p) | Self::Adopted(p) => p,
        }
    }

    pub fn into_path(self) -> PathBuf {
        match self {
            Self::Created(p) | Self::Adopted(p) => p,
        }
    }
}

/// `FG-REPO-0004` with the reason adoption was refused (never the tokens).
fn not_adoptable(id: RepoId, path: &Path, reason: &'static str) -> ForgeError {
    occupied(id, path).with_context("adopt_refused", reason)
}

impl StorageRoot {
    /// The idempotent form of [`StorageRoot::init_bare`] for the provisioning
    /// job: create the repository, or — when the path already holds a real
    /// bare repository under the root marked with this `token` — adopt it and
    /// re-apply the settings and hooks path. A symlink, a file, an unmarked
    /// or foreign-token directory, or one git cannot read stays
    /// `FG-REPO-0004`. Temporary directories left by a crashed attempt do not
    /// interfere (each attempt uses its own name).
    pub async fn init_bare_or_adopt(
        &self,
        git: &Git,
        id: RepoId,
        token: Uuid,
        hooks_path: Option<&Path>,
    ) -> Result<Provisioned, ForgeError> {
        let hooks = hooks_value(hooks_path)?;
        let (path, shard) = self.prepare_shard(id).await?;
        let Some(meta) = self.occupant(&path, id).await? else {
            return self
                .create(git, id, token, hooks.as_deref(), &path, &shard)
                .await
                .map(Provisioned::Created);
        };
        if meta.file_type().is_symlink() {
            return Err(not_adoptable(id, &path, "symlink"));
        }
        if !meta.is_dir() {
            return Err(not_adoptable(id, &path, "not_dir"));
        }
        let path = self.existing(id)?;
        self.check_marker(git, id, token, &path).await?;
        configure(git, &path, hooks.as_deref()).await?;
        Ok(Provisioned::Adopted(path))
    }

    /// The directory is a bare repository whose own `config` (a regular
    /// file, not a symlink out of the root) carries `token`.
    async fn check_marker(
        &self,
        git: &Git,
        id: RepoId,
        token: Uuid,
        path: &Path,
    ) -> Result<(), ForgeError> {
        match tokio::fs::symlink_metadata(path.join("config")).await {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Err(not_adoptable(id, path, "config_not_a_file")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(not_adoptable(id, path, "not_a_repository"))
            }
            Err(e) => return Err(io_failed("inspect", e).with_context("repo_id", id)),
        }
        let marker = git
            .command("config")
            .repo(path)
            .arg("--local")
            .arg("--get")
            .arg("--default=")
            .arg(PROVISION_TOKEN_KEY)
            .output()
            .await
            .map_err(|e| unreadable(id, path, e))?
            .stdout_str();
        let marker = marker.trim();
        if marker.is_empty() {
            return Err(not_adoptable(id, path, "unmarked"));
        }
        if Uuid::parse_str(marker).ok() != Some(token) {
            return Err(not_adoptable(id, path, "token_mismatch"));
        }
        let bare = git
            .command("rev-parse")
            .repo(path)
            .arg("--is-bare-repository")
            .output()
            .await
            .map_err(|e| unreadable(id, path, e))?
            .stdout_str();
        if bare.trim() != "true" {
            return Err(not_adoptable(id, path, "not_bare"));
        }
        Ok(())
    }
}

/// git refused the directory (broken config, not a repository): it is not
/// ours to adopt. Transient failures (timeout, busy, git missing) keep their
/// own code so the job retries.
fn unreadable(id: RepoId, path: &Path, e: ForgeError) -> ForgeError {
    if e.code() == ErrorCode::GitProcessFailed {
        not_adoptable(id, path, "unreadable").with_source(e)
    } else {
        e
    }
}
