//! The AI asset draft of a merge request, as the merge request screen shows
//! it (plan 17 P17-10, ADR-0039 Amendment P17-10b): `GET
//! …/merge-requests/{iid}/ai-draft`, read like the merge request
//! (`MergeRequestRead`).
//!
//! The facts are the worker's own record of its proposal
//! (`forge_db::llm::asset_draft::latest_proposed`: the branch, the commit
//! over the merge request's head, the changed asset paths, the count of
//! the model's edits the checks dropped) — never the model's words — and
//! the branch's head now (the browse API), so the screen says when the
//! draft branch was deleted or moved since, and when a draft branch exists
//! without its record (`unrecorded`: the worker's record failed). A merge request from any branch
//! but `improve/<id>` has no draft. Reading writes nothing; the draft
//! decides nothing (a person applies it).

use forge_core::improve_draft::ai_draft_branch_for;
use forge_core::{ProjectAccessLoader as _, RepositoryState};
use forge_errors::{ErrorCode, ForgeError};

use crate::Review;

/// What the screens call the draft (text, never only a colour).
pub const AI_DRAFT_LABEL: &str = "AI の下書き（提案） / AI draft (suggestion)";

/// The draft branch now, against the recorded commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchState {
    /// The branch's head is the recorded draft commit.
    Current,
    /// The branch exists with another head (someone pushed to it).
    Moved,
    /// The branch no longer exists (deleted: a new draft may follow).
    Deleted,
}

impl BranchState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Moved => "moved",
            Self::Deleted => "deleted",
        }
    }
}

/// The recorded draft and its branch now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownDraft {
    pub draft: forge_db::llm::asset_draft::ProposedDraft,
    pub branch_state: BranchState,
    /// The branch's head now (`None`: deleted).
    pub branch_head: Option<String>,
}

/// `GET …/ai-draft`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiDraftView {
    pub iid: i64,
    /// The merge request comes from `improve/<id>` (only those get drafts).
    pub improvement: bool,
    /// `None`: no draft was proposed.
    pub draft: Option<ShownDraft>,
    /// No proposal is recorded but the draft branch exists (its full
    /// name): the screen says so instead of "no draft".
    pub unrecorded_branch: Option<String>,
}

impl Review {
    /// The AI draft of the merge request `proof` read (module docs).
    pub async fn ai_draft(
        &self,
        proof: &forge_auth::Authorized<forge_auth::MergeRequestCtx>,
    ) -> Result<AiDraftView, ForgeError> {
        use forge_auth::Action;
        if proof.action() != Action::MergeRequestRead {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "proof_action_mismatch")
                .with_context("action", proof.action().name()));
        }
        let (project, Some(iid)) = *proof.id() else {
            return Err(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_shape_mismatch")
            );
        };
        let mr = forge_db::review::get(&self.db, project, iid)
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "decided_merge_request_missing")
            })?;
        let Some(branch) = ai_draft_branch_for(&mr.source_branch) else {
            return Ok(AiDraftView {
                iid,
                improvement: false,
                draft: None,
                unrecorded_branch: None,
            });
        };
        let draft = forge_db::llm::asset_draft::latest_proposed(&self.db, project, mr.id).await?;
        if draft.as_ref().is_some_and(|d| d.branch != branch.as_str()) {
            // The record names another branch than this merge request's
            // draft branch: never shown as this one's.
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "ai_draft_branch_mismatch")
                .with_context("merge_request_id", mr.id.get()));
        }
        let record = self.db.load_project(project, None).await?.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "decided_project_missing")
        })?;
        let head = if record.repository.state == RepositoryState::Ready {
            let key = proof
                .principal()
                .user()
                .map_or_else(|| "anonymous".to_owned(), |u| format!("user:{}", u.get()));
            match self
                .branches
                .branch_head(record.repository.id, &branch, &key)
                .await
            {
                Ok(head) => Some(head.as_str().to_owned()),
                // The branch is not there (a person deleted it).
                Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => None,
                Err(e) => return Err(e.with_context("branch", "ai_draft")),
            }
        } else {
            None
        };
        let Some(draft) = draft else {
            // A draft branch without its record (the worker's record failed,
            // or a person created the branch): said so, never "no draft".
            return Ok(AiDraftView {
                iid,
                improvement: true,
                draft: None,
                unrecorded_branch: head.map(|_| branch.as_str().to_owned()),
            });
        };
        let branch_state = match head.as_deref() {
            Some(h) if h == draft.commit_sha => BranchState::Current,
            Some(_) => BranchState::Moved,
            None => BranchState::Deleted,
        };
        Ok(AiDraftView {
            iid,
            improvement: true,
            draft: Some(ShownDraft {
                draft,
                branch_state,
                branch_head: head,
            }),
            unrecorded_branch: None,
        })
    }
}
