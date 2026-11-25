//! The `search.index_documents` job handler (plan 20 K4, ADR-0040
//! Amendment K4): one run of [`forge_checks::doc_search::DocumentIndexer`]
//! — the default branch's documents into the knowledge search. Enqueued by
//! forge-db's `search-index` subscriber on every push that moved a branch,
//! and by `forge search reindex` (full).
//!
//! Success is logged once by forge-checks (`forge.search.documents_indexed`);
//! a failure is returned to the job runner, which logs it. Retry (the
//! queue's backoff): git timeouts / busy / process failures, storage I/O,
//! the database, a repository still being provisioned. On the last
//! attempt, or a failure that can never succeed, the project is recorded
//! `failed` with `FG-KNOW-0002 reason=read_failed` first, so the gap is
//! visible in `search_index_state` (runbook search-index); a malformed
//! payload is dead at once.

use std::sync::Arc;

use forge_checks::doc_search::DocumentIndexer;
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::search::{documents_payload, DOCUMENTS_JOB};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};

use super::provision::Storage;

/// The handler of `search.index_documents`.
pub(crate) struct SearchDocumentsHandler {
    kind: JobKind,
    pub(crate) indexer: Arc<DocumentIndexer>,
}

impl SearchDocumentsHandler {
    pub(crate) fn new(db: &Db, storage: &Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(DOCUMENTS_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", DOCUMENTS_JOB)
        })?;
        let browse = Browser::new(
            storage.git.clone(),  // clone: the browser owns its handles
            storage.root.clone(), // clone: as git
            BrowseLimits::default(),
        );
        Ok(Self {
            kind,
            indexer: Arc::new(DocumentIndexer::new(db.clone(), browse)), // clone: pool handles
        })
    }
}

impl JobHandler for SearchDocumentsHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let indexer = Arc::clone(&self.indexer);
        Box::pin(async move {
            let request = documents_payload(&job.payload).map_err(JobFailure::permanent)?;
            let request_id = job.request_id();
            let Err(e) = indexer.run(request, &request_id).await else {
                return Ok(());
            };
            let retryable = super::review::retryable(&e);
            let last = job.lease.attempt >= job.max_attempts;
            if retryable && !last {
                return Err(JobFailure::retry(e));
            }
            // Failed for good: visible in the state table, then dead.
            indexer.fail(request.project, &request_id, &e).await;
            Err(if retryable {
                JobFailure::retry(e)
            } else {
                JobFailure::permanent(e)
            })
        })
    }
}

#[cfg(test)]
#[path = "search_documents_tests.rs"]
mod tests;
