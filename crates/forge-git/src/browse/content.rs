//! File content (see [`super`]): blobs, raw files and archives.

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};

use super::path::RepoPath;
use super::tree::TreeEntry;
use super::{not_resolved, Browser, Deadline};
use crate::revision::Revision;
use crate::stream::GitStream;

/// A blob's head for the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub entry: TreeEntry,
    pub size: u64,
    /// At most [`BrowseLimits::max_blob_bytes`].
    pub content: Vec<u8>,
    pub truncated: bool,
}

/// A raw file: read whole (small), or streamed.
#[derive(Debug)]
pub enum RawBody {
    Bytes(Vec<u8>),
    Stream(GitStream),
}

/// A raw file of a commit.
#[derive(Debug)]
pub struct RawBlob {
    pub size: u64,
    pub oid: String,
    pub body: RawBody,
}

/// The archive formats (`git archive --format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    TarGz,
}

impl ArchiveFormat {
    /// Split `name.zip` / `name.tar.gz` (the extension is removed once).
    pub fn split(name: &str) -> Option<(&str, Self)> {
        if let Some(stem) = name.strip_suffix(".tar.gz") {
            return Some((stem, Self::TarGz));
        }
        name.strip_suffix(".zip").map(|stem| (stem, Self::Zip))
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::TarGz => "tar.gz",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Zip => "application/zip",
            Self::TarGz => "application/gzip",
        }
    }

    fn git_format(self) -> &'static str {
        match self {
            Self::Zip => "--format=zip",
            Self::TarGz => "--format=tar.gz",
        }
    }
}

impl Browser {
    /// The blob entry at `path` (`not_a_blob` for a tree or a submodule).
    pub(super) async fn blob_entry(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
        deadline: Deadline,
    ) -> Result<TreeEntry, ForgeError> {
        if path.is_root() {
            return Err(not_resolved("not_a_blob"));
        }
        match self.entry_at(repo, commit, path, deadline).await? {
            None => Err(not_resolved("path_not_found")),
            Some(e) if e.kind.is_blob() => Ok(e),
            Some(_) => Err(not_resolved("not_a_blob")),
        }
    }

    /// The blob entry at `path` (id, mode, size) without reading it (a
    /// raw `HEAD`).
    pub async fn raw_entry(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
    ) -> Result<TreeEntry, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        self.blob_entry(repo, commit, path, deadline).await
    }

    /// The first [`BrowseLimits::max_blob_bytes`] of the blob at `path`.
    pub async fn blob(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
    ) -> Result<Blob, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let entry = self.blob_entry(repo, commit, path, deadline).await?;
        let size = entry.blob_size()?;
        let dir = self.storage.existing(repo)?;
        let _slot = self.claim(repo)?;
        let (out, truncated) = self
            .git
            .command("cat-file")
            .arg("blob")
            .revision(Revision::object_id(&entry.oid)?)
            .repo(&dir)
            .timeout(deadline.left()?)
            .output_truncated(self.limits.max_blob_bytes)
            .await?;
        Ok(Blob {
            entry,
            size,
            content: out.stdout,
            truncated,
        })
    }

    /// The blob at `path`, whole (≤ `raw_inline_max_bytes`) or as a stream
    /// (`principal`: the stream cap key, e.g. `user:<id>`).
    pub async fn raw(
        &self,
        repo: RepoId,
        commit: &str,
        path: &RepoPath,
        principal: &str,
    ) -> Result<RawBlob, ForgeError> {
        let deadline = Deadline::new(self.limits.timeout);
        let entry = self.blob_entry(repo, commit, path, deadline).await?;
        let size = entry.blob_size()?;
        let dir = self.storage.existing(repo)?;
        let cmd = self
            .git
            .command("cat-file")
            .arg("blob")
            .revision(Revision::object_id(&entry.oid)?)
            .repo(&dir);
        if size <= self.limits.raw_inline_max_bytes {
            let _slot = self.claim(repo)?;
            let cap = usize::try_from(size).unwrap_or(usize::MAX);
            let (out, truncated) = cmd
                .timeout(deadline.left()?)
                .output_truncated(cap.saturating_add(1))
                .await?;
            if truncated || out.stdout.len() != cap {
                return Err(ForgeError::new(ErrorCode::GitProcessFailed)
                    .with_context("reason", "blob_size_mismatch")
                    .with_context("repo_id", repo.get())
                    .with_context("expected_bytes", size)
                    .with_context("read_bytes", out.stdout.len()));
            }
            return Ok(RawBlob {
                size,
                oid: entry.oid,
                body: RawBody::Bytes(out.stdout),
            });
        }
        let stream = cmd
            .idle_timeout(self.limits.stream_idle)
            .timeout(self.limits.stream_total)
            .spawn_stream(principal)
            .await?;
        Ok(RawBlob {
            size,
            oid: entry.oid,
            body: RawBody::Stream(stream),
        })
    }

    /// `git archive` of `commit` as a stream, every file below `prefix/`
    /// (`prefix`: a forge-made name, `[a-z0-9._-]`, not starting with `-`;
    /// forge-web builds it from the project's path segment, which
    /// `PathSegment::parse` keeps lower-case, plus `-` and hex digits).
    pub async fn archive(
        &self,
        repo: RepoId,
        commit: &str,
        format: ArchiveFormat,
        prefix: &str,
        principal: &str,
    ) -> Result<GitStream, ForgeError> {
        let valid = !prefix.is_empty()
            && prefix.len() <= 200
            && !prefix.starts_with('-')
            && prefix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
        if !valid {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "archive_prefix_invalid"));
        }
        let dir = self.storage.existing(repo)?;
        self.git
            .command("archive")
            // gzip inside git (2.38+), never a configured filter command.
            .config_override("tar.tar.gz.command", "git archive gzip")
            .config_override("tar.tgz.command", "git archive gzip")
            .arg(format.git_format())
            .arg("--prefix")
            .arg_owned(format!("{prefix}/"))
            .revision(Revision::object_id(commit)?)
            .repo(&dir)
            .idle_timeout(self.limits.stream_idle)
            .timeout(self.limits.stream_total)
            .spawn_stream(principal)
            .await
    }
}
