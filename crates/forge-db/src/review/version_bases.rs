//! The bases of a merge request version (M3 R2, ADR-0024 §1): the target's
//! tip and the merge base when the version job recorded the version.

use forge_core::ids::{MergeRequestId, ProjectId};
use forge_core::{AuditDetails, MergeRequestVersionChange, ObjectId};
use forge_errors::ForgeError;

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome};
use crate::{query_error, Db};

fn audit_entry(project: ProjectId, details: &AuditDetails) -> AuditEntry {
    AuditEntry {
        action: details.action(),
        outcome: Outcome::Succeeded,
        resource_type: "project",
        resource_id: Some(project.get()),
        project_id: Some(project),
        error_code: None,
        details: details.to_value(),
    }
}

/// Record version `version`'s bases (M3 R2, ADR-0024 §1): `base` the
/// target's tip and `merge_base` the merge base at the time the version job
/// recorded it. Set once, together: a version whose `base_sha` is already
/// set is left alone (`Ok(false)`, nothing written), so an older version
/// keeps its bases when the target moves later. One audit-only
/// `merge_request.version_bases_recorded` row (actor `ctx`: the worker's
/// `system`). `Ok(false)` also when the version does not exist.
pub async fn record_version_bases(
    db: &Db,
    ctx: &ChangeContext,
    mr: MergeRequestId,
    version: i64,
    base: &ObjectId,
    merge_base: Option<&ObjectId>,
) -> Result<bool, ForgeError> {
    const ACTION: &str = "merge_request.version_bases_recorded";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let written: Option<(i64, i64)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "UPDATE forge.merge_request_versions v SET base_sha = $3, merge_base_sha = $4 \
                     FROM forge.merge_requests m WHERE v.merge_request_id = $1 \
                     AND v.version_no = $2 AND v.base_sha IS NULL AND v.merge_base_sha IS NULL \
                     AND m.id = v.merge_request_id RETURNING m.project_id, m.iid",
                )
                .bind(mr.get())
                .bind(version)
                .bind(base.as_str())
                .bind(merge_base.map(ObjectId::as_str))
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("review.version_bases", e))
            })
            .await?;
        let Some((project, iid)) = written else {
            return Ok(crate::change::Settle::Discard(false));
        };
        let project = ProjectId::new(project);
        let details = AuditDetails::MergeRequestVersion(MergeRequestVersionChange::BasesRecorded {
            merge_request: mr,
            iid,
            version,
            base_sha: base.clone(), // clone: the details own their values
            merge_base_sha: merge_base.cloned(), // clone: as above
        });
        let record = ChangeRecord {
            audit: audit_entry(project, &details),
            affected: Vec::new(),
            events: Vec::new(),
        };
        Ok(crate::change::Settle::Commit(record, true))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
