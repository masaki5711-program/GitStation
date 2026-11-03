//! The version job's carry decision (M3 R6, ADR-0024 §8.2): before the new
//! head is recorded, decide which live approvals the new head may carry
//! ([`crate::approvals::carry::may_carry_approvals`]). R6 review round 2:
//! the change is measured from the head each approval was given on
//! (`forge_db::review::approved_heads`, the `carried_from` chain followed
//! back), so the limits are cumulative over every carried push — never
//! per push. The git reads run only when the project keeps approvals on
//! push and the target is unprotected; every read is bounded by the refs
//! lock's deadline. A git failure or an exhausted deadline is "no carry"
//! (the approvals end, the default), logged once; only the database is an
//! error. forge-db decides again under the merge request's lock
//! (`record_mr_version_carrying`).

use forge_core::{ObjectId, RepoId, RequestId};
use forge_db::review::{CarryPlan, StoredMergeRequest};
use forge_db::Db;
use forge_errors::ForgeError;
use forge_git::review_changes::ChangeListing;
use forge_git::review_inputs::CodeownersFile;

use crate::approvals::carry::{
    may_carry_approvals, CarryChanges, CarryCodeowners, CarryInputs, CARRY_MAX_FILES,
};
use crate::approvals::codeowners;
use crate::version_refs::{git_budget, Deadline, VersionRefs};

/// The approved heads whose change to `tip` may carry their approvals, for
/// the recorded head (`None`: nothing is carried).
pub(crate) async fn carry_plan(
    db: &Db,
    refs: &VersionRefs,
    deadline: Deadline,
    repo: RepoId,
    stored: &StoredMergeRequest,
    tip: &ObjectId,
    request_id: &RequestId,
) -> Result<Option<CarryPlan>, ForgeError> {
    let Some(previous) = stored
        .head_sha
        .as_deref()
        .map(ObjectId::parse)
        .transpose()?
    else {
        return Ok(None);
    };
    if &previous == tip {
        return Ok(None);
    }
    let settings = forge_db::review::merge_settings(db, stored.project_id).await?;
    if settings.values.reset_approvals_on_push {
        // The default: nothing to read, nothing to log.
        return Ok(None);
    }
    let rules = forge_db::protected_refs::rules_for(db, stored.project_id).await?;
    if !rules.levels(&stored.target_branch).is_empty() {
        log_decision(
            request_id,
            stored,
            (&previous, None, tip),
            Err("target_protected"),
        );
        return Ok(None);
    }
    let mut approved: Vec<ObjectId> = Vec::new();
    for a in forge_db::review::approved_heads(db, stored.id).await? {
        match a.approved_head {
            Some(head) => {
                if !approved.contains(&head) {
                    approved.push(head);
                }
            }
            // Longer than MAX_CARRY_HOPS: never carried (it ends).
            None => log_decision(
                request_id,
                stored,
                (&previous, None, tip),
                Err("carry_chain_too_long"),
            ),
        }
    }
    if approved.is_empty() {
        return Ok(None);
    }
    let codeowners = target_codeowners(refs, deadline, repo, stored, request_id).await;
    let mut allowed = Vec::with_capacity(approved.len());
    for head in approved {
        let changes = if &head == tip {
            // Back to the head the approval was given on: nothing new.
            CarryChanges::Known(Vec::new())
        } else {
            changed(refs, deadline, repo, &head, tip, request_id).await
        };
        let inputs = CarryInputs {
            reset_approvals_on_push: false,
            target_protected: false,
            changes,
            codeowners: codeowners.clone(), // clone: one decision per approved head
        };
        let decided = tokio::task::spawn_blocking(move || may_carry_approvals(&inputs))
            .await
            .unwrap_or(Err("decision_failed"));
        log_decision(request_id, stored, (&previous, Some(&head), tip), decided);
        if decided.is_ok() {
            allowed.push(head);
        }
    }
    Ok((!allowed.is_empty()).then_some(CarryPlan {
        from: previous,
        approved_heads: allowed,
    }))
}

fn log_decision(
    request_id: &RequestId,
    stored: &StoredMergeRequest,
    (from, approved_head, to): (&ObjectId, Option<&ObjectId>, &ObjectId),
    decided: Result<(), &'static str>,
) {
    tracing::info!(
        target: "forge_review",
        event = "forge.review.approvals_carry_decided",
        request_id = %request_id,
        merge_request_id = stored.id.get(),
        project_id = stored.project_id.get(),
        from = from.as_str(),
        approved_head = approved_head.map(ObjectId::as_str),
        to = to.as_str(),
        carry = decided.is_ok(),
        reason = decided.err().unwrap_or("allowed"),
        "whether a new head carries the approvals given on approved_head (reset_approvals_on_push = false)"
    );
}

/// The entries changed from `from` to `to` with their modes and line
/// counts, at most one past [`CARRY_MAX_FILES`] (unknown on any failure:
/// never carried).
async fn changed(
    refs: &VersionRefs,
    deadline: Deadline,
    repo: RepoId,
    from: &ObjectId,
    to: &ObjectId,
    request_id: &RequestId,
) -> CarryChanges {
    let Ok(timeout) = git_budget(deadline, "carry_changed_entries") else {
        return CarryChanges::Unknown("timeout");
    };
    let (git, storage) = refs.git();
    match git
        .changed_entries(storage, repo, from, to, CARRY_MAX_FILES + 1, timeout)
        .await
    {
        Ok(ChangeListing::Complete(entries)) => CarryChanges::Known(entries),
        Ok(ChangeListing::TooMany) => CarryChanges::Unknown("too_many_files"),
        Ok(ChangeListing::NonUtf8) => CarryChanges::Unknown("non_utf8_path"),
        Err(e) => {
            warn_unread(request_id, "changed_entries", &e);
            CarryChanges::Unknown("git_failed")
        }
    }
}

/// The target tip's CODEOWNERS (unknown on any failure).
async fn target_codeowners(
    refs: &VersionRefs,
    deadline: Deadline,
    repo: RepoId,
    stored: &StoredMergeRequest,
    request_id: &RequestId,
) -> CarryCodeowners {
    let tip = match refs.branch_tip(repo, &stored.target_branch, deadline).await {
        Ok(Some(tip)) => tip,
        Ok(None) => return CarryCodeowners::Unknown,
        Err(e) => {
            warn_unread(request_id, "target_tip", &e);
            return CarryCodeowners::Unknown;
        }
    };
    let Ok(timeout) = git_budget(deadline, "carry_codeowners") else {
        return CarryCodeowners::Unknown;
    };
    let (git, storage) = refs.git();
    match git
        .codeowners_file(storage, repo, &tip, codeowners::MAX_BYTES, timeout)
        .await
    {
        Ok(CodeownersFile::Absent) => CarryCodeowners::Absent,
        Ok(CodeownersFile::Found { bytes, .. }) => match codeowners::parse(&bytes) {
            Ok(file) => CarryCodeowners::Parsed(file),
            Err(_) => CarryCodeowners::Unknown,
        },
        Ok(CodeownersFile::TooLarge { .. } | CodeownersFile::NotAFile { .. }) => {
            CarryCodeowners::Unknown
        }
        Err(e) => {
            warn_unread(request_id, "codeowners", &e);
            CarryCodeowners::Unknown
        }
    }
}

fn warn_unread(request_id: &RequestId, input: &'static str, e: &ForgeError) {
    tracing::warn!(
        target: "forge_review",
        event = "forge.review.approvals_carry_unread",
        error_code = e.code().id(),
        request_id = %request_id,
        input,
        error = %e,
        "an input of the approval carry could not be read; the approvals end (no carry)"
    );
}
