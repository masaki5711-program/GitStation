//! CI artifacts (M4 C6, plan 14 §4.1, §4.2, §9 #6; ADR-0029 decision 5).
//!
//! - **Store** ([`ArtifactStore`]): `<storage.root>/ci-artifacts/<project_id>/
//!   <artifact_id>`. forge names every file from the ids, never from the
//!   path the job sent (that is a row's `path`). An upload is written to
//!   `<id>.part` (created new, never over another file), its sha256 and
//!   size measured while it arrives, bounded by
//!   `forge_core::ci_artifact::MAX_FILE_BYTES` and what the attempt may
//!   still store (`FG-CI-0019`), synced, renamed into place, then recorded
//!   — a failure anywhere removes what was written.
//! - **Upload** ([`Pipelines::begin_artifact`], `JobArtifactUpload`): the
//!   job's own token, while the job runs.
//! - **Read by a person or an agent** ([`Pipelines::artifacts`],
//!   [`Pipelines::artifact_file`], `CiArtifactRead`): decided like the job's
//!   log (`authorize_ci_output`, `route=ci_artifact`): an AI agent audience
//!   only when the pipeline's mark and the default branch's tip are both
//!   `Open`; no row is read before the decision.
//! - **Read by a later job** ([`Pipelines::needed_artifacts`],
//!   [`Pipelines::needed_artifact_file`], `JobArtifactDownload`): the job's
//!   own token; only the current, succeeded attempt of a job it `needs` in
//!   its own pipeline.
//! - **Expiry** ([`expire_artifacts`], the worker): after 30 days, unless
//!   `keep` (a deployed version, C11): the file first, then the row.

use std::path::{Path, PathBuf};

use forge_auth::{authorize_ci_output, Action, Authorized, JobSelfCtx, ProjectCtx};
use forge_core::ci_artifact::{self, MAX_FILE_BYTES};
use forge_core::RequestId;
use forge_db::ci_artifacts::{self as db, StoredArtifact, UploadTarget};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::outputs::{pipeline_mark, OutputView};
use crate::{require, Pipelines};

pub use forge_db::ci_artifacts::StoredArtifact as Artifact;

/// The directory under the storage root.
pub const DIR: &str = "ci-artifacts";
/// Artifacts one expiry pass removes at most.
pub const EXPIRY_BATCH: i64 = 200;

/// Where the artifact files live.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

fn storage_failed(operation: &'static str, e: std::io::Error) -> ForgeError {
    ForgeError::new(ErrorCode::StorageIoFailed)
        .with_context("operation", operation)
        .with_source(e)
}

impl ArtifactStore {
    /// The store under `storage_root` (`storage.root`).
    pub fn new(storage_root: &Path) -> Self {
        Self {
            root: storage_root.join(DIR),
        }
    }

    fn dir(&self, project: i64) -> PathBuf {
        self.root.join(project.to_string())
    }

    /// The file of artifact `id` of `project`.
    pub fn file(&self, project: i64, id: i64) -> PathBuf {
        self.dir(project).join(id.to_string())
    }

    /// Remove the directories of projects that no longer exist (the purge
    /// deleted their rows): how many were removed. Only numeric directory
    /// names are looked at.
    pub async fn remove_purged(
        &self,
        db_handle: &Db,
        request_id: &RequestId,
    ) -> Result<usize, ForgeError> {
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(storage_failed("artifact_root_list", e)),
        };
        let mut ids = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| storage_failed("artifact_root_list", e))?
        {
            if let Some(id) = entry
                .file_name()
                .to_str()
                .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|n| n.parse::<i64>().ok())
            {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            return Ok(0);
        }
        let known = db::known_projects(db_handle, &ids).await?;
        let mut removed = 0;
        for id in ids.into_iter().filter(|id| !known.contains(id)) {
            tokio::fs::remove_dir_all(self.dir(id)).await.map_err(|e| {
                storage_failed("artifact_project_remove", e).with_context("project_id", id)
            })?;
            removed += 1;
            tracing::info!(
                target: "forge_pipelines",
                event = "forge.ci.artifacts_of_purged_project_removed",
                request_id = %request_id,
                project_id = id,
                "the artifact files of a purged project were removed"
            );
        }
        Ok(removed)
    }
}

/// An upload under way: write the body, then [`ArtifactUpload::finish`]
/// or [`ArtifactUpload::abort`].
pub struct ArtifactUpload {
    target: UploadTarget,
    path: String,
    part: PathBuf,
    dir_project: i64,
    file: tokio::fs::File,
    hasher: Sha256,
    size: u64,
    limit: u64,
}

impl std::fmt::Debug for ArtifactUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactUpload")
            .field("job_id", &self.target.job_id)
            .field("part", &self.part)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// What an upload stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUpload {
    pub id: i64,
    pub job_id: i64,
    pub attempt: i32,
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
}

fn too_large(job: i64, limit: u64) -> ForgeError {
    ForgeError::new(ErrorCode::ArtifactTooLarge)
        .with_context("reason", "file_limit")
        .with_context("job_id", job)
        .with_context("limit_bytes", limit)
}

impl ArtifactUpload {
    /// Take the next part of the body. `FG-CI-0019` past the bound (the
    /// caller aborts).
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), ForgeError> {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.size.saturating_add(len) > self.limit {
            return Err(too_large(self.target.job_id, self.limit));
        }
        self.file
            .write_all(bytes)
            .await
            .map_err(|e| storage_failed("artifact_write", e))?;
        self.hasher.update(bytes);
        self.size += len;
        Ok(())
    }

    /// Sync, record, then put in place (named by the new row's id). A
    /// failure removes the file and the row.
    pub async fn finish(self, pipelines: &Pipelines) -> Result<StoredUpload, ForgeError> {
        if let Err(e) = self.file.sync_all().await {
            remove_quietly(&self.part).await;
            return Err(storage_failed("artifact_sync", e));
        }
        let digest: [u8; 32] = self.hasher.finalize().into();
        let id = match db::record(&pipelines.db, &self.target, &self.path, self.size, &digest).await
        {
            Ok(id) => id,
            Err(e) => {
                remove_quietly(&self.part).await;
                return Err(e);
            }
        };
        let done = pipelines.store()?.file(self.dir_project, id);
        if let Err(e) = tokio::fs::rename(&self.part, &done).await {
            remove_quietly(&self.part).await;
            let e = storage_failed("artifact_rename", e).with_context("artifact_id", id);
            if let Err(gone) = db::discard(&pipelines.db, id).await {
                // Both logged: the row stays without its file (a read
                // answers 404; the expiry removes the row).
                tracing::error!(
                    target: "forge_pipelines",
                    event = "forge.ci.artifact_discard_failed",
                    error_code = gone.code().id(),
                    artifact_id = id,
                    error = %gone,
                    "an artifact row without its file was not removed"
                );
            }
            return Err(e);
        }
        Ok(StoredUpload {
            id,
            job_id: self.target.job_id,
            attempt: self.target.attempt,
            path: self.path,
            size_bytes: self.size,
            sha256: digest.iter().map(|b| format!("{b:02x}")).collect(),
        })
    }

    /// Give up: the part file is removed.
    pub async fn abort(self) {
        drop(self.file);
        remove_quietly(&self.part).await;
    }
}

/// Remove a file of a failed upload; a failure is logged once here (the
/// expiry never sees a file without a row, so it is left for the operator:
/// runbook ci-runner).
async fn remove_quietly(file: &Path) {
    if let Err(e) = tokio::fs::remove_file(file).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                target: "forge_pipelines",
                event = "forge.ci.artifact_remove_failed",
                error_code = ErrorCode::StorageIoFailed.id(),
                file = %file.display(),
                error = %e,
                "a failed upload's file was not removed"
            );
        }
    }
}

/// A file name no other upload uses: `<job>-<nanos>-<n>.part` (created
/// new, never over another file).
fn part_name(job: i64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{job}-{nanos}-{}.part",
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

impl Pipelines {
    pub(crate) fn store(&self) -> Result<&ArtifactStore, ForgeError> {
        self.artifacts.as_ref().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "artifacts_unconfigured")
        })
    }

    /// `PUT /api/v1/runner/jobs/{job_id}/artifacts?path=` (`JobArtifactUpload`,
    /// the job's own token): an upload of `path`. Errors: `FG-WEB-0006`
    /// (`param=path` and its reason, `artifact_exists`), `FG-CI-0004` (the
    /// job is not running), `FG-CI-0019` (the attempt is full), storage.
    pub async fn begin_artifact(
        &self,
        proof: &Authorized<JobSelfCtx>,
        path: &str,
    ) -> Result<ArtifactUpload, ForgeError> {
        require(proof.action(), Action::JobArtifactUpload)?;
        let job = proof.ctx().job();
        if let Some(reason) = ci_artifact::check_path(path) {
            return Err(ForgeError::new(ErrorCode::RequestInvalid)
                .with_context("param", "path")
                .with_context("reason", reason));
        }
        let target = db::upload_target(&self.db, job, path)
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::JobLeaseLost)
                    .with_context("job_id", job)
                    .with_context("operation", "artifact")
            })?;
        let store = self.store()?;
        let dir = store.dir(target.project.get());
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| storage_failed("artifact_dir", e))?;
        let part = dir.join(part_name(job));
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .await
            .map_err(|e| storage_failed("artifact_create", e))?;
        Ok(ArtifactUpload {
            limit: MAX_FILE_BYTES.min(target.bytes_left),
            dir_project: target.project.get(),
            target,
            path: path.to_owned(),
            part,
            file,
            hasher: Sha256::new(),
            size: 0,
        })
    }

    /// `GET …/pipelines/{pipeline_id}/jobs/{job_id}/artifacts`
    /// (`CiArtifactRead`): decided on the marks first (module docs).
    /// `None`: no such job in the project.
    pub async fn artifacts(
        &self,
        proof: &Authorized<ProjectCtx>,
        view: OutputView,
        ids: (i64, i64),
    ) -> Result<Option<Vec<StoredArtifact>>, ForgeError> {
        require(proof.action(), Action::CiArtifactRead)?;
        let read = db::list_decided(&self.db, *proof.id(), ids, |target| {
            decide(proof, view, target)
        })
        .await?;
        Ok(read.map(|(rows, _allowed)| rows))
    }

    /// `GET …/jobs/{job_id}/artifacts/{artifact_id}` (`CiArtifactRead`):
    /// the row and its file, decided as [`Pipelines::artifacts`].
    pub async fn artifact_file(
        &self,
        proof: &Authorized<ProjectCtx>,
        view: OutputView,
        ids: (i64, i64),
        artifact: i64,
    ) -> Result<Option<(StoredArtifact, PathBuf)>, ForgeError> {
        require(proof.action(), Action::CiArtifactRead)?;
        let read = db::one_decided(&self.db, *proof.id(), ids, artifact, |target| {
            decide(proof, view, target)
        })
        .await?;
        let Some((row, _allowed)) = read else {
            return Ok(None);
        };
        let file = self.store()?.file(row.project_id, row.id);
        Ok(Some((row, file)))
    }

    /// `GET /api/v1/runner/jobs/{job_id}/needs/artifacts`
    /// (`JobArtifactDownload`, the job's own token): the artifacts of the
    /// jobs it `needs` (module docs).
    pub async fn needed_artifacts(
        &self,
        proof: &Authorized<JobSelfCtx>,
    ) -> Result<Vec<StoredArtifact>, ForgeError> {
        require(proof.action(), Action::JobArtifactDownload)?;
        db::needs_of(&self.db, proof.ctx().job()).await
    }

    /// `GET /api/v1/runner/jobs/{job_id}/needs/artifacts/{artifact_id}`:
    /// one of [`Pipelines::needed_artifacts`] and its file.
    pub async fn needed_artifact_file(
        &self,
        proof: &Authorized<JobSelfCtx>,
        artifact: i64,
    ) -> Result<Option<(StoredArtifact, PathBuf)>, ForgeError> {
        require(proof.action(), Action::JobArtifactDownload)?;
        let Some(row) = db::need_one(&self.db, proof.ctx().job(), artifact).await? else {
            return Ok(None);
        };
        let file = self.store()?.file(row.project_id, row.id);
        Ok(Some((row, file)))
    }
}

fn decide(
    proof: &Authorized<ProjectCtx>,
    view: OutputView,
    target: &db::ArtifactTarget,
) -> Result<forge_auth::CiOutputAllowed, ForgeError> {
    authorize_ci_output(
        proof,
        pipeline_mark(target.copy_mark),
        view.tip_mark,
        view.declared_agent,
    )
    .map_err(|e| {
        e.with_context("pipeline_id", target.pipeline_id)
            .with_context("job_id", target.job_id)
    })
}

/// One expiry pass (the worker): each expired artifact (not `keep`) loses
/// its file, then its row. A file already gone is fine; one that cannot be
/// removed keeps its row (the next pass tries again) and the first such
/// error is returned after the pass (the worker's supervisor logs it), the
/// others logged here once each. Returns how many were removed.
pub async fn expire_artifacts(
    db_handle: &Db,
    store: &ArtifactStore,
    request_id: &RequestId,
) -> Result<usize, ForgeError> {
    let due = db::due(db_handle, EXPIRY_BATCH).await?;
    let mut removed = 0;
    let mut first: Option<ForgeError> = None;
    for (id, project) in due {
        let file = store.file(project, id);
        let gone = match tokio::fs::remove_file(&file).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(storage_failed("artifact_expire", e).with_context("artifact_id", id)),
        };
        let done = match gone {
            Ok(()) => db::forget(db_handle, id).await.map(|_| ()),
            Err(e) => Err(e),
        };
        match done {
            Ok(()) => removed += 1,
            Err(e) if first.is_none() => first = Some(e),
            Err(e) => tracing::error!(
                target: "forge_pipelines",
                event = "forge.ci.artifact_expiry_failed",
                request_id = %request_id,
                error_code = e.code().id(),
                artifact_id = id,
                error = %e,
                "an expired artifact was not removed"
            ),
        }
    }
    if removed > 0 {
        tracing::info!(
            target: "forge_pipelines",
            event = "forge.ci.artifacts_expired",
            request_id = %request_id,
            removed,
            "expired CI artifacts removed"
        );
    }
    match first {
        Some(e) => Err(e),
        None => Ok(removed),
    }
}
