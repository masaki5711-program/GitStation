//! The optional source branch deletion after a merge (ADR-0024 §4.2 step
//! 10): decided as the requester's branch delete — `RepoPush` on
//! `Resource::Ref { update: Delete }` with the push path's protected-ref
//! rules, through `authorize_ref_updates` — then a compare-and-swap delete
//! on the merged head. A protected source, a role or scope that does not
//! allow the delete (a token holding only `merge_execute` has no
//! `write_repository`), or a branch that moved past the merged head is
//! kept and the attempt says why (`source_outcome`). A failure never undoes
//! the merge: it is logged once (warn) with its code.

use forge_auth::{authorize_ref_updates, prove, Action, ProjectCtx, RefUpdate, RefUpdateCtx};
use forge_core::SourceOutcome;
use forge_db::review::merges::{self, GateOutcome, GateStep, SourceDeletion, StoredMerge};
use forge_errors::ForgeError;
use forge_git::RefChange;

use super::execute::{is_refusal, requester_ctx, system_ctx, Subject};
use super::{job_principal, Merger};
use crate::loaded;

impl Merger {
    /// Delete the source branch of applied attempt `attempt` (module docs);
    /// every outcome is recorded on the attempt, failures are logged.
    pub(super) async fn delete_source(&self, attempt: &StoredMerge, subject: &Subject) {
        let outcome = match self.try_delete_source(attempt, subject).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!(
                    target: "forge_review",
                    event = "forge.review.merge_source_kept",
                    error_code = e.code().id(),
                    request_id = %attempt.request_id,
                    merge_id = attempt.id,
                    r#ref = subject.mr.source_branch.as_str(),
                    error = %e,
                    "the source branch was not deleted; the merge stands"
                );
                SourceOutcome::KeptFailed
            }
        };
        if outcome == SourceOutcome::Deleted {
            return;
        }
        if let Err(e) = merges::set_source_outcome(&self.db, attempt.id, outcome).await {
            tracing::warn!(
                target: "forge_review",
                event = "forge.review.merge_source_outcome_unrecorded",
                error_code = e.code().id(),
                request_id = %attempt.request_id,
                merge_id = attempt.id,
                outcome = outcome.as_str(),
                error = %e,
                "the source branch outcome could not be recorded"
            );
        }
    }

    async fn try_delete_source(
        &self,
        attempt: &StoredMerge,
        subject: &Subject,
    ) -> Result<SourceOutcome, ForgeError> {
        let source = &subject.mr.source_branch;
        let decided = merges::merge_gate_decided(
            &self.db,
            &system_ctx(attempt),
            attempt.id,
            GateStep::Facts,
            |g| {
                let principal =
                    job_principal(attempt.requested_by, g.facts.account, &g.credential)?;
                let project = prove(
                    &principal,
                    Action::RepoPush,
                    ProjectCtx::new(loaded(&g.facts.project)),
                )?;
                let update = RefUpdateCtx::new(source.clone(), RefUpdate::Delete, &g.protection); // clone: the context owns its name
                authorize_ref_updates(&project, std::slice::from_ref(&update))?
                    .into_iter()
                    .collect::<Result<Vec<()>, ForgeError>>()
                    .map(drop)
            },
        )
        .await;
        match decided {
            Ok(GateOutcome::Decided(())) => {}
            Ok(GateOutcome::Moved(_)) => return Ok(SourceOutcome::KeptFailed),
            Err(refusal) if is_refusal(&refusal) => {
                tracing::info!(
                    target: "forge_review",
                    event = "forge.review.merge_source_kept",
                    error_code = refusal.code().id(),
                    request_id = %attempt.request_id,
                    merge_id = attempt.id,
                    reason = "refused",
                );
                return Ok(SourceOutcome::KeptRefused);
            }
            Err(e) => return Err(e),
        }
        let tip = self
            .git
            .read_ref(&self.storage, subject.repo, source, self.timeouts.read)
            .await?;
        let Some(old) = tip.filter(|t| *t == attempt.head_sha) else {
            return Ok(SourceOutcome::KeptMoved);
        };
        let change = RefChange::Delete {
            name: source.clone(), // clone: the change owns its name
            old: old.clone(),     // clone: as above
        };
        let deleted = self
            .git
            .update_refs(
                &self.storage,
                subject.repo,
                &[change],
                self.timeouts.update_ref,
            )
            .await;
        self.invalidate(subject.repo);
        deleted?;
        tracing::info!(
            target: "forge_review",
            event = "forge.review.merge_source_deleted_applied",
            request_id = %attempt.request_id,
            merge_id = attempt.id,
            r#ref = source.as_str(),
            old = old.as_str(),
        );
        let deletion = SourceDeletion {
            repo: subject.repo,
            source_branch: source.clone(), // clone: the record owns its name
            old,
        };
        if let Err(e) = merges::record_source_deleted(
            &self.db,
            &requester_ctx(attempt, None),
            attempt,
            subject.mr.iid,
            &deletion,
        )
        .await
        {
            // The branch is gone: its record failed (FG-DB-0001 and the like).
            tracing::warn!(
                target: "forge_review",
                event = "forge.review.merge_source_unrecorded",
                error_code = e.code().id(),
                request_id = %attempt.request_id,
                merge_id = attempt.id,
                r#ref = source.as_str(),
                error = %e,
                "the source branch was deleted but its record failed"
            );
            if let Err(e2) =
                merges::set_source_outcome(&self.db, attempt.id, SourceOutcome::Deleted).await
            {
                tracing::warn!(
                    target: "forge_review",
                    event = "forge.review.merge_source_outcome_unrecorded",
                    error_code = e2.code().id(),
                    request_id = %attempt.request_id,
                    merge_id = attempt.id,
                    error = %e2,
                    "the source branch outcome could not be recorded"
                );
            }
        }
        Ok(SourceOutcome::Deleted)
    }
}
