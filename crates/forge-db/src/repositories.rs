//! Repositories (step D4, ADR-0004, ADR-0009): one per project in M1. A
//! repository row is written by `projects::create` in state `provisioning`
//! together with the job that creates its bare directory (D4 option A);
//! the job's handler, `mark_ready` and the provisioning report follow in
//! D4 MR3b.

use std::time::Duration;

use forge_core::ids::RepoId;
use forge_core::{DedupKey, DedupPart, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use crate::jobs::{self, JobKind, JobTimeout, MaxAttempts, NewJob};

/// The job kind that creates a repository's bare directory
/// (`init_bare_or_adopt`, then `mark_ready`). Payload: `{"repo_id": <id>}`.
pub const PROVISION_JOB: &str = "repository.provision";
/// One run of the provisioning job (step D plan: 60 s).
pub const PROVISION_TIMEOUT: Duration = Duration::from_secs(60);
/// The prefix of its dedup key, `repository_provision:<repo_id>`
/// (`<aggregate>_<verb>`, ADR-0016 Amendment D4).
pub const PROVISION_DEDUP_PREFIX: &str = "repository_provision";

/// `FG-SYS-0001` for a constant that does not fit its type (a bug here).
fn constant(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

/// The provisioning job of `repo`, for the request that created it. Its
/// dedup key is built with [`DedupKey::new`] from the repository id only,
/// so a second enqueue for the same repository is deduplicated while the
/// first job is queued, running or succeeded (a dead one does not hold it:
/// `forge admin repo reprovision`, MR3b, enqueues again).
pub(crate) fn provision_job(repo: RepoId, request_id: &RequestId) -> Result<NewJob, ForgeError> {
    let kind = JobKind::parse(PROVISION_JOB).ok_or_else(|| constant("invalid_job_kind"))?;
    let timeout =
        JobTimeout::new(PROVISION_TIMEOUT).ok_or_else(|| constant("invalid_job_timeout"))?;
    let key = DedupKey::new(PROVISION_DEDUP_PREFIX, &[DedupPart::Id(repo.get())])?;
    let dedup_key =
        jobs::DedupKey::parse(key.as_str()).ok_or_else(|| constant("invalid_job_dedup_key"))?;
    Ok(NewJob {
        kind,
        payload: serde_json::json!({ "repo_id": repo.get() }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout,
        dedup_key: Some(dedup_key),
        origin_request_id: request_id.clone(),
        origin_event_seq: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provision_job_carries_the_repo_id_and_its_dedup_key() {
        let request = RequestId::parse("req-7").expect("request id");
        let job = provision_job(RepoId::new(42), &request).expect("job");
        assert_eq!(job.kind.as_str(), "repository.provision");
        assert_eq!(job.payload, serde_json::json!({ "repo_id": 42 }));
        assert_eq!(
            job.dedup_key.as_ref().map(jobs::DedupKey::as_str),
            Some("repository_provision:42")
        );
        assert_eq!(job.timeout.get(), Duration::from_secs(60));
        assert_eq!(job.max_attempts, MaxAttempts::DEFAULT);
        assert_eq!(job.origin_request_id.as_str(), "req-7");
    }
}
