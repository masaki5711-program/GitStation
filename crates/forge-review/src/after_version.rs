//! The version job's request of a mergeability check after a version (M3
//! R2, ADR-0024 §1, §3): the head version's bases, then the check unless
//! the stored status already answers the pair (see `mergeability`).

use forge_core::ids::MergeRequestId;
use forge_core::{CheckReason, MergeabilityStatus, ObjectId, RepoId, RequestId};
use forge_db::review::{self, RefsLock};
use forge_db::Db;
use forge_errors::ForgeError;

use crate::mergeability::system;
use crate::versions::VersionRefs;

/// The version job's step 5: the head version's bases, then a
/// mergeability check unless the stored status answers (head, target tip)
/// already — a decision, or a `check_failed` whose reason a rerun would
/// repeat (`CheckReason::is_deterministic`; git or job trouble never
/// answers). The check's seq when one was requested.
pub(crate) async fn after_version(
    db: &Db,
    refs: &VersionRefs,
    lock: &RefsLock,
    mr: MergeRequestId,
    repo: RepoId,
    request_id: &RequestId,
) -> Result<Option<i64>, ForgeError> {
    let Some(now) = review::refresh_target(db, mr).await? else {
        return Ok(None);
    };
    let current = &now.merge_request;
    if current.state != forge_core::MergeRequestState::Open {
        return Ok(None);
    }
    let Some(head) = current
        .head_sha
        .as_deref()
        .map(ObjectId::parse)
        .transpose()?
    else {
        return Ok(None);
    };
    let tip = refs
        .branch_tip(repo, &current.target_branch, lock.deadline())
        .await?;
    if let Some(tip) = &tip {
        let versions = review::list_versions(db, mr).await?;
        let unrecorded = versions
            .iter()
            .find(|v| v.head_sha == head)
            .filter(|v| v.base_sha.is_none() && v.merge_base_sha.is_none());
        if let Some(version) = unrecorded {
            let merge_base = refs.merge_base(repo, tip, &head, lock.deadline()).await?;
            review::record_version_bases(
                db,
                &system(request_id),
                mr,
                version.version_no,
                tip,
                merge_base.as_ref(),
            )
            .await?;
        }
    }
    let stored = review::mergeability_written(db, mr).await?;
    let answered = stored.is_some_and(|s| {
        let answers = match s.status {
            MergeabilityStatus::CanBeMerged | MergeabilityStatus::CannotBeMerged => true,
            MergeabilityStatus::CheckFailed => s.reason.is_some_and(CheckReason::is_deterministic),
            MergeabilityStatus::Unchecked | MergeabilityStatus::Checking => false,
        };
        answers && s.checked_head_sha.as_ref() == Some(&head) && s.checked_target_sha == tip
    });
    if answered {
        return Ok(None);
    }
    review::request_check(db, mr, request_id.clone()).await // clone: the job keeps its origin
}
