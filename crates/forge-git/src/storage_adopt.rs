//! Provisioning retry (D4): adopt a repository an earlier attempt of the same
//! provisioning job created, identified by its `forge.provisionToken` marker.
//! Anything else at the path stays `FG-REPO-0004`, so a restored database
//! never adopts a directory that is not its own.
//!
//! Assumption (ADR-0009 amendment): the storage root is writable only by
//! forge. The marker and the config are re-checked right before every write,
//! which leaves a residual check-to-write window accepted under it.

use std::io;
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
    /// An earlier (or concurrent) attempt with the same token created it; it
    /// was adopted and its configuration re-applied.
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

/// git exiting non-zero (broken config, not a repository, unwritable) or
/// printing an oversized config on an existing directory is permanent:
/// `FG-REPO-0004`. Transient failures (timeout, busy, git missing) keep their
/// own code so the job retries. Both the read (`check_marker`) and the write
/// (`configure`) step follow this rule.
fn permanent_git_failure(
    id: RepoId,
    path: &Path,
    reason: &'static str,
    e: ForgeError,
) -> ForgeError {
    if matches!(
        e.code(),
        ErrorCode::GitProcessFailed | ErrorCode::GitOutputTooLarge
    ) {
        not_adoptable(id, path, reason).with_source(e)
    } else {
        e
    }
}

impl StorageRoot {
    /// The idempotent form of [`StorageRoot::init_bare`] for the provisioning
    /// job: create the repository, or — when the path already holds a real
    /// bare repository under the root marked (once, in its own config, no
    /// includes) with this `token` — adopt it and re-apply the settings and
    /// hooks path. A concurrent attempt that loses the rename adopts the same
    /// way. Anything else stays `FG-REPO-0004` with `adopt_refused`.
    /// Temporary directories left by a crashed attempt do not interfere.
    /// Which errors to retry: [`super::provision_error_is_retryable`].
    pub async fn init_bare_or_adopt(
        &self,
        git: &Git,
        id: RepoId,
        token: Uuid,
        hooks_path: Option<&Path>,
    ) -> Result<Provisioned, ForgeError> {
        let hooks = hooks_value(hooks_path)?;
        let (path, shard) = self.prepare_shard(id).await?;
        if self.occupant(&path, id).await?.is_none() {
            match self
                .create(git, id, token, hooks.as_deref(), &path, &shard)
                .await
            {
                Ok(created) => return Ok(Provisioned::Created(created)),
                // Lost the rename to a concurrent attempt: adopt once below.
                Err(e) if e.code() == ErrorCode::RepoPathOccupied => {}
                Err(e) => return Err(e),
            }
        }
        self.adopt(git, id, token, hooks.as_deref(), &path)
            .await
            .map(Provisioned::Adopted)
    }

    async fn adopt(
        &self,
        git: &Git,
        id: RepoId,
        token: Uuid,
        hooks: Option<&str>,
        path: &Path,
    ) -> Result<PathBuf, ForgeError> {
        let Some(meta) = self.occupant(path, id).await? else {
            // It vanished after the rename race: a retry can create it.
            return Err(io_failed(
                "inspect",
                io::Error::other("repository vanished during adoption"),
            )
            .with_context("repo_id", id));
        };
        if meta.file_type().is_symlink() {
            return Err(not_adoptable(id, path, "symlink"));
        }
        if !meta.is_dir() {
            return Err(not_adoptable(id, path, "not_dir"));
        }
        let path = self.existing(id)?;
        let config = regular_config(id, &path).await?;
        check_marker(git, id, token, &path, &config).await?;
        // Re-check right before writing: `--file` writes the path given, and
        // a config swapped for a symlink since the first check is refused.
        let config = regular_config(id, &path).await?;
        configure(git, &config, hooks)
            .await
            .map_err(|e| permanent_git_failure(id, &path, "unwritable", e))?;
        Ok(path)
    }
}

/// `<repo>/config` is a regular file (not a symlink out of the root).
async fn regular_config(id: RepoId, path: &Path) -> Result<PathBuf, ForgeError> {
    let config = path.join("config");
    match tokio::fs::symlink_metadata(&config).await {
        Ok(meta) if meta.is_file() => Ok(config),
        Ok(_) => Err(not_adoptable(id, path, "config_not_a_file")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(not_adoptable(id, path, "not_a_repository"))
        }
        // Not ours to fix by retrying: an operator must look.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            Err(not_adoptable(id, path, "unreadable").with_source(e))
        }
        Err(e) => Err(io_failed("inspect", e).with_context("repo_id", id)),
    }
}

/// The repository's own config (no includes) carries exactly one marker,
/// equal to `token`, has no `include.*` / `includeIf.*`, and is bare.
async fn check_marker(
    git: &Git,
    id: RepoId,
    token: Uuid,
    path: &Path,
    config: &Path,
) -> Result<(), ForgeError> {
    let listing = git
        .command("config")
        .arg("--file")
        .arg_owned(config)
        .arg("--no-includes")
        .arg("--null")
        .arg("--list")
        .output()
        .await
        .map_err(|e| permanent_git_failure(id, path, "unreadable", e))?
        .stdout_str();
    let mut markers = Vec::new();
    for entry in listing.split('\0').filter(|e| !e.is_empty()) {
        let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
        let key = key.to_ascii_lowercase();
        if key.starts_with("include.") || key.starts_with("includeif.") {
            return Err(not_adoptable(id, path, "includes"));
        }
        if key.eq_ignore_ascii_case(PROVISION_TOKEN_KEY) {
            markers.push(value.trim().to_string());
        }
    }
    match markers.as_slice() {
        [] => return Err(not_adoptable(id, path, "unmarked")),
        [marker] if Uuid::parse_str(marker).ok() == Some(token) => {}
        [_] => return Err(not_adoptable(id, path, "token_mismatch")),
        _ => return Err(not_adoptable(id, path, "duplicate_marker")),
    }
    let bare = git
        .command("rev-parse")
        .repo(path)
        .arg("--is-bare-repository")
        .output()
        .await
        .map_err(|e| permanent_git_failure(id, path, "unreadable", e))?
        .stdout_str();
    if bare.trim() != "true" {
        return Err(not_adoptable(id, path, "not_bare"));
    }
    Ok(())
}
