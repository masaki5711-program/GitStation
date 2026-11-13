//! Specification releases (M4 C12, plan 04 §6 `spec-publish`, plan 03
//! §5.1 "版の公開（タグ）"): a version-shaped tag pushed at a commit of the
//! default branch publishes the specification at that commit — one
//! `spec.released` event for the integrations (Kanflow).
//!
//! - The `ci-pipelines` subscriber enqueues one `spec.publish` job per
//!   version-shaped tag a push **created** ([`publish_job`], deduplicated
//!   per (project, tag, object)).
//! - The job (forge-pipelines `spec_publish`, run by `forge worker`) checks
//!   with git that the tag's commit is on the default branch and holds the
//!   repository's own specification directory, then [`record_release`]:
//!   one audit row (`spec.released`, actor `system`) and the event, in one
//!   transaction, deduplicated by `spec_released:<project>:<commit>:<tag
//!   hash>` (a retried job writes nothing twice).

use forge_core::dedup_key::DedupPart;
use forge_core::pipeline::PipelineChange;
use forge_core::{
    AuditDetails, DedupKey, EventType, ObjectId, ProjectId, RefName, RepoId, RequestId, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use sha2::{Digest, Sha256};

use crate::change::Outcome;
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Committed, DomainEvent};
use crate::jobs::{self, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::Db;

/// The kind of the job that decides and records one release.
pub const SPEC_PUBLISH_JOB: &str = "spec.publish";
/// One decision: a few git reads (the browse timeout each) and one write.
pub const SPEC_PUBLISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// The most tags one push publishes (the rest are logged by the
/// subscriber).
pub const MAX_PUSH_RELEASES: usize = 10;
const DEDUP_PREFIX: &str = "spec_released";
/// The audit resource and the event's aggregate.
const RESOURCE: &str = "project";

/// One created tag to consider (the job's payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRequest {
    pub project: ProjectId,
    /// `refs/tags/…`, version-shaped.
    pub tag: RefName,
    /// What the tag pointed at in the push (a commit, or an annotated tag
    /// object: the job peels it).
    pub object: ObjectId,
    /// The pusher (a person's or a service account's push).
    pub released_by: Option<UserId>,
}

fn invalid_job(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", SPEC_PUBLISH_JOB)
}

fn tag_hash(tag: &RefName) -> [u8; 32] {
    Sha256::digest(tag.as_str().as_bytes()).into()
}

/// The job for `request`: one per (project, tag, object).
pub fn publish_job(
    request: &PublishRequest,
    origin: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let hash = tag_hash(&request.tag);
    let hex: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let key = format!(
        "spec_publish:{}:{}:{hex}",
        request.project.get(),
        request.object.as_str()
    );
    Ok(NewJob {
        kind: JobKind::parse(SPEC_PUBLISH_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({
            "project_id": request.project.get(),
            "tag": request.tag.as_str(),
            "object": request.object.as_str(),
            "released_by": request.released_by.map(UserId::get),
        }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(SPEC_PUBLISH_TIMEOUT).ok_or_else(|| invalid_job("job_timeout"))?,
        dedup_key: Some(jobs::DedupKey::parse(&key).ok_or_else(|| invalid_job("dedup_key"))?),
        origin_request_id: origin,
        origin_event_seq,
    })
}

/// The payload of a claimed `spec.publish` job (`FG-SYS-0001
/// reason=invalid_job_payload` otherwise).
pub fn publish_payload(payload: &serde_json::Value) -> Result<PublishRequest, ForgeError> {
    let bad = || invalid_job("invalid_job_payload");
    let project = payload
        .get("project_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(bad)?;
    let tag = payload
        .get("tag")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| RefName::parse(raw).ok())
        .filter(|r| r.as_str().starts_with("refs/tags/"))
        .ok_or_else(bad)?;
    let object = payload
        .get("object")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| ObjectId::parse(raw).ok())
        .ok_or_else(bad)?;
    let released_by = match payload.get("released_by") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(UserId::new(
            v.as_i64().filter(|id| *id > 0).ok_or_else(bad)?,
        )),
    };
    Ok(PublishRequest {
        project: ProjectId::new(project),
        tag,
        object,
        released_by,
    })
}

/// A release to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRelease {
    pub project: ProjectId,
    pub repository: RepoId,
    pub tag: RefName,
    /// The tag's name without `refs/tags/` (`v1.5`).
    pub version: String,
    /// The commit the tag peels to.
    pub commit: ObjectId,
    pub released_by: Option<UserId>,
    /// The default branch it was found on (`refs/heads/…`).
    pub default_branch: String,
}

/// Write the `spec.released` audit row and event (actor `system`). `false`:
/// the same release was recorded before (a retried job): nothing written.
pub async fn record_release(
    db: &Db,
    ctx: &ChangeContext,
    release: &NewRelease,
) -> Result<bool, ForgeError> {
    let change = PipelineChange::SpecReleased {
        project: release.project,
        repository: release.repository.get(),
        tag: release.tag.as_str().to_owned(),
        version: release.version.clone(), // clone: the change owns it
        sha: release.commit.as_str().to_owned(),
        released_by: release.released_by.map(UserId::get),
        default_branch: release.default_branch.clone(), // clone: as version
    };
    let details = AuditDetails::Pipeline(change);
    let value = details.to_value();
    let hash = tag_hash(&release.tag);
    let record = ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(release.project.get()),
            project_id: Some(release.project),
            error_code: None,
            details: value.clone(), // clone: the same JSON is the event payload
        },
        affected: Vec::new(),
        events: vec![DomainEvent {
            event_type: EventType::SpecReleased,
            schema_version: 1,
            aggregate_type: RESOURCE,
            aggregate_id: release.project.get(),
            project_id: Some(release.project),
            payload: value,
            dedup_key: Some(DedupKey::new(
                DEDUP_PREFIX,
                &[
                    DedupPart::Id(release.project.get()),
                    DedupPart::Object(&release.commit),
                    DedupPart::Hash(&hash),
                ],
            )?),
        }],
    };
    let tx = ChangeTx::begin(db, ctx).await?;
    match tx.commit(record).await? {
        Committed::Recorded { .. } => Ok(true),
        Committed::Deduped => Ok(false),
    }
}

#[cfg(test)]
#[path = "spec_releases_tests.rs"]
mod tests;
