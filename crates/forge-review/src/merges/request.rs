//! `POST …/merge-requests/{iid}/merge` (ADR-0024 §4.2 step 1): the merge
//! request's entry. Nothing here runs a git command but the browse API's
//! branch lookups; the merge itself is the job's.

use forge_auth::{Action, Authorized, MergeRequestCtx, RefProtection};
use forge_core::{MergeMethod, ObjectId, ProjectRepository};
use forge_db::review::merges::{self, CredentialRef, NewMerge, StoredMerge};
use forge_db::review::{self, StoredMergeability};
use forge_errors::{ErrorCode, ForgeError};

use super::evaluate::{blocked, evaluate_merge, stored_mergeable, BlockReason, MergeFacts};
use crate::{
    account, caller, change_context, loaded, locked, require, shape_mismatch, Review, ReviewRequest,
};

/// What the requester asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeAsk {
    /// The head the requester saw (required, ADR-0024 §4.2 step 1).
    pub sha: ObjectId,
    /// `None`: the project's default method.
    pub method: Option<MergeMethod>,
    /// `None`: the project's default.
    pub delete_source_branch: Option<bool>,
    /// The commit message (`None`: built from the merge request's title and
    /// description). 1 character to 64 KiB.
    pub message: Option<String>,
}

fn message_invalid(rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "param_invalid")
        .with_context("param", "message")
        .with_context("rule", rule)
}

/// The message rules: 1 character to 64 KiB, no NUL (git refuses it).
fn check_message(message: &str) -> Result<(), ForgeError> {
    if message.trim().is_empty() {
        return Err(message_invalid("empty"));
    }
    if message.len() > forge_git::commit::MAX_MESSAGE_BYTES {
        return Err(message_invalid("too_long"));
    }
    if message.contains('\0') {
        return Err(message_invalid("nul"));
    }
    Ok(())
}

/// `FG-REVIEW-0008 reason=<reason>` with the head expected and seen.
pub(crate) fn head_changed(reason: &'static str, expected: &str, actual: &str) -> ForgeError {
    ForgeError::new(ErrorCode::HeadChanged)
        .with_context("reason", reason)
        .with_context("expected", expected.to_owned())
        .with_context("actual", actual.to_owned())
}

/// Whether the stored check is worth a new request: none, a stale
/// decision, or a failed check (a check in flight is left alone).
fn wants_check(stored: Option<&StoredMergeability>) -> bool {
    use forge_core::MergeabilityStatus as S;
    match stored.map(|s| s.status) {
        None | Some(S::CanBeMerged | S::CannotBeMerged | S::CheckFailed) => true,
        Some(S::Unchecked | S::Checking) => false,
    }
}

impl Review {
    /// The protection in effect on `target` of `project` (the push path's
    /// rules, strictest first; `None`: unprotected) — what the merge route's
    /// loader decides with (ADR-0024 §4.3).
    pub async fn target_protection(
        &self,
        project: forge_core::ProjectId,
        target: &forge_core::RefName,
    ) -> Result<Option<RefProtection>, ForgeError> {
        let rules = forge_db::protected_refs::rules_for(&self.db, project).await?;
        Ok(RefProtection::of(&rules, target))
    }

    /// The latest merge attempt of merge request `mr` (`None`: never asked).
    pub async fn latest_merge(
        &self,
        mr: forge_core::MergeRequestId,
    ) -> Result<Option<StoredMerge>, ForgeError> {
        merges::latest_merge(&self.db, mr).await
    }

    /// `POST …/merge-requests/{iid}/merge` (`MergeRequestMerge`): the
    /// method must be allowed (`FG-REVIEW-0010`), the message valid
    /// (`FG-WEB-0006`), `sha` the merge request's head (`FG-REVIEW-0008
    /// reason=stale_view`); then, under the review locks, the decision is
    /// made again with the target's protection as stored now, a live
    /// attempt is `FG-REVIEW-0011`, and `evaluate_merge()` must find
    /// nothing (`FG-REVIEW-0007` with every reason; `mergeability_unknown`
    /// requests a new check so a retry can pass). Then the `queued` attempt
    /// and its job are written. `credential` is the request's own session
    /// or token (its id only).
    pub async fn request_merge(
        &self,
        request: &ReviewRequest,
        credential: CredentialRef,
        proof: &Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
        ask: &MergeAsk,
    ) -> Result<StoredMerge, ForgeError> {
        require(proof.action(), Action::MergeRequestMerge)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("merge_request_merge"));
        };
        // M3 R6: the project's settings (read again in the gate below).
        let settings = super::conditions::settings_of(
            &forge_db::review::merge_settings(&self.db, project).await?,
        );
        let method = ask.method.unwrap_or_else(|| settings.default_method());
        settings.check(method)?;
        if let Some(message) = &ask.message {
            check_message(message)?;
        }
        let user = caller(proof)?;
        let current = self
            .merge_request(project, iid)
            .await?
            .ok_or_else(crate::absent)?;
        let recorded = current.head_sha.as_deref().unwrap_or("");
        if recorded != ask.sha.as_str() {
            return Err(head_changed("stale_view", recorded, ask.sha.as_str())
                .with_context("merge_request_id", current.id.get()));
        }
        // The live tips, through the browse API's ref list (no new git).
        let mut tips = Vec::with_capacity(2);
        for (which, name) in [
            ("source_branch", &current.source_branch),
            ("target_branch", &current.target_branch),
        ] {
            let found = self
                .require_branches(project, repo, user, &[(which, name)])
                .await;
            tips.push(match found {
                Ok(mut heads) => heads.pop(),
                Err(e) if is_ref_not_found(&e) => None,
                Err(e) => return Err(e),
            });
        }
        let target_tip = tips.pop().flatten();
        let source_tip = tips.pop().flatten();
        // M3 R5: the approval evaluation's git facts at the requested head
        // into the live target tip (the target's CODEOWNERS, never the
        // merge request's own).
        let asker = crate::approvals::Asker {
            request_id: &request.request_id,
            project,
            repo: crate::ready(project, repo)?,
        };
        let approval_git = self
            .review_git()?
            .facts(
                &self.db,
                asker,
                &current,
                &ask.sha,
                target_tip.as_ref(),
                &mut crate::approvals::Budget::new(crate::approvals::REQUEST_BUDGET),
            )
            .await?;
        let ctx = change_context(request, proof.principal(), user);
        let new = NewMerge {
            method,
            head_sha: ask.sha.clone(), // clone: forge-db owns its request
            message: ask.message.clone(), // clone: as above
            delete_source_branch: ask
                .delete_source_branch
                .unwrap_or_else(|| settings.delete_source_default()),
            credential,
        };
        let sha = &ask.sha;
        let wanted_check = std::sync::atomic::AtomicBool::new(false);
        let decided = merges::request_merge_mr_decided(
            &self.db,
            &ctx,
            (project, iid),
            user,
            &new,
            |gate| {
                let mr = locked(&gate.facts)?;
                let protection = RefProtection::of(&gate.protection, &mr.target_branch);
                let fresh = MergeRequestCtx::merging(loaded(&gate.facts.project), mr, protection);
                proof
                    .reconfirm(account(gate.facts.account), fresh)
                    .map(drop)?;
                let stored = gate
                    .facts
                    .merge_request
                    .as_ref()
                    .ok_or_else(crate::absent)?;
                if stored.head_sha.as_deref() != Some(sha.as_str()) {
                    return Err(head_changed(
                        "stale_view",
                        stored.head_sha.as_deref().unwrap_or(""),
                        sha.as_str(),
                    ));
                }
                if gate.live.is_some() {
                    // forge-db answers FG-REVIEW-0011 for it.
                    return Ok(());
                }
                // M3 R6: the settings as locked allow the method still.
                super::conditions::method_allowed(&gate.approvals.settings, method)?;
                let conditions = super::conditions::conditions(
                    &gate.protection,
                    &stored.target_branch,
                    sha,
                    gate.unresolved_discussions,
                    &super::conditions::NoCheckRuns,
                );
                let facts = MergeFacts {
                    state: stored.state,
                    draft: stored.draft,
                    merge_in_progress: false,
                    source_present: source_tip.is_some(),
                    mergeable: Some(stored_mergeable(
                        gate.mergeability.as_ref(),
                        sha,
                        target_tip.as_ref(),
                    )),
                    method,
                    // Computed by the job under the lock.
                    ff_possible: None,
                    approvals: approval_git
                        .clone()
                        .inputs(stored, sha, gate.approvals.clone()), // clone: the evaluation owns its inputs
                    checks: conditions.checks,
                    discussions: conditions.discussions,
                };
                evaluate_merge(&facts).map(drop).map_err(|reasons| {
                    if reasons.contains(&BlockReason::MergeabilityUnknown)
                        && wants_check(gate.mergeability.as_ref())
                    {
                        wanted_check.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    blocked(&reasons)
                })
            },
            // Review round 1: the default message is frozen at the request
            // from the row as locked (a later title / description edit never
            // reaches the commit).
            |mr| default_message(method, mr),
        )
        .await;
        if wanted_check.load(std::sync::atomic::Ordering::Relaxed)
            && current.state == forge_core::MergeRequestState::Open
        {
            // A merge waiting on the check asks for one (ADR-0024 R2: "マー
            // ジの要求のときに積むのは R4"); failing to ask is logged, the
            // answer is the refusal either way.
            if let Err(e) =
                review::request_check(&self.db, current.id, request.request_id.clone()).await
            {
                tracing::warn!(
                    target: "forge_review",
                    event = "forge.review.merge_check_request_failed",
                    error_code = e.code().id(),
                    request_id = %request.request_id,
                    merge_request_id = current.id.get(),
                    error = %e,
                    "could not request a mergeability check for a blocked merge"
                );
            }
        }
        let (stored, ()) =
            decided.map_err(|e| e.with_context("merge_request_id", current.id.get()))?;
        Ok(stored)
    }
}

/// The lookup answered "no such branch" (`FG-REPO-0023 reason=ref_not_found`).
pub(crate) fn is_ref_not_found(e: &ForgeError) -> bool {
    e.context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "ref_not_found")
}

/// The default message of a merge (ADR-0024 §4.1: from the merge request's
/// title and description, never from the client's identity), cut to
/// [`MAX_MESSAGE_BYTES`] on a character boundary.
pub(super) fn default_message(
    method: MergeMethod,
    mr: &forge_db::review::StoredMergeRequest,
) -> String {
    let source = mr
        .source_branch
        .branch()
        .unwrap_or(mr.source_branch.as_str());
    let target = mr
        .target_branch
        .branch()
        .unwrap_or(mr.target_branch.as_str());
    let mut text = match method {
        MergeMethod::MergeCommit => format!(
            "Merge branch '{source}' into '{target}'\n\n{}\n\nMerge request !{}\n",
            mr.title, mr.iid
        ),
        MergeMethod::Squash | MergeMethod::FastForward => format!("{}\n", mr.title),
    };
    if !mr.description.trim().is_empty() {
        text.push('\n');
        text.push_str(mr.description.trim_end());
        text.push('\n');
    }
    if text.len() > forge_git::commit::MAX_MESSAGE_BYTES {
        let mut end = forge_git::commit::MAX_MESSAGE_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}
