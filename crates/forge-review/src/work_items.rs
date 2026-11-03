//! A person's work item links on a merge request (M3 I1b, `WorkItemLink`):
//! `PUT|DELETE …/merge-requests/{iid}/work-items/{ref}`. The registry
//! proved the action on what the loader read; the command decides again
//! inside its transaction on the rows as locked (`Authorized::reconfirm`),
//! like every review command. Reading the links is
//! [`Review::work_item_links`] (`MergeRequestRead`).

use forge_auth::{Action, Authorized, MergeRequestCtx};
use forge_core::{MergeRequestId, WorkItemRef};
use forge_db::review;
use forge_db::work_items::{self, Links};
use forge_errors::ForgeError;

pub use forge_db::review::{LinkAnswer, LinkOutcome};

use crate::{
    account, caller, change_context, loaded, locked, require, shape_mismatch, Review, ReviewRequest,
};

impl Review {
    /// `PUT …/work-items/{ref}`: link `reference` (the project's binding's
    /// work item). [`LinkOutcome::Unchanged`] when it is linked already;
    /// `FG-INT-0018` for an unbound project, `FG-INT-0019` past the limit.
    pub async fn link_work_item(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
        reference: &WorkItemRef,
    ) -> Result<(LinkOutcome, LinkAnswer), ForgeError> {
        require(proof.action(), Action::WorkItemLink)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("work_item_link"));
        };
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (outcome, answer, ()) = review::link_work_item_mr_decided(
            &self.db,
            &ctx,
            (project, iid),
            reference,
            user,
            |facts| {
                let fresh = MergeRequestCtx::new(loaded(&facts.project), locked(facts)?);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        Ok((outcome, answer))
    }

    /// `DELETE …/work-items/{ref}`: end the open link of `reference` (a
    /// detected or a person's); the row stays. `FG-REPO-0005
    /// resource=work_item_link` when it is not linked now.
    pub async fn unlink_work_item(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
        reference: &WorkItemRef,
    ) -> Result<LinkAnswer, ForgeError> {
        require(proof.action(), Action::WorkItemLink)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("work_item_unlink"));
        };
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (answer, ()) = review::unlink_work_item_mr_decided(
            &self.db,
            &ctx,
            (project, iid),
            reference,
            user,
            |facts| {
                let fresh = MergeRequestCtx::new(loaded(&facts.project), locked(facts)?);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        Ok(answer)
    }

    /// The links (open and ended) of merge request `id`, oldest first (the
    /// route proved `MergeRequestRead` on it).
    pub async fn work_item_links(&self, id: MergeRequestId) -> Result<Links, ForgeError> {
        work_items::links(&self.db, id).await
    }
}
