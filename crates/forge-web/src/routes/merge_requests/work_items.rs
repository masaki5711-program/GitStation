//! A merge request's work item links (M3 I1b, plan 02 §2, §5.7):
//!
//! - `GET …/merge-requests/{iid}/work-items` (`MergeRequestRead`): every
//!   link, open and ended (history), oldest first — detected ones
//!   (`source = auto`, from the title and the source branch, by the
//!   `work-item-sync` subscriber) and a person's (`manual`).
//! - `PUT …/merge-requests/{iid}/work-items/{ref}` (`WorkItemLink`:
//!   Developer and above) → `200` with `outcome` = `linked` / `unchanged`.
//! - `DELETE …/work-items/{ref}` (`WorkItemLink`) → `204`; the row stays
//!   with `unlinked_at`, and detection does not link it again.
//!
//! `{ref}` is the canonical `PBI-<n>` (`FG-WEB-0006 param=ref rule=format`
//! otherwise, before anything is looked up); the work item is the one of
//! the project's binding (`FG-INT-0018` when the project is not bound).
//! forge-review decides again inside the transaction.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{MergeRequestCtx, MergeRequestEdit};
use forge_core::{parse_positive_id, ProjectId, UserId, WorkItem, WorkItemRef};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::work_items::{LinkAnswer, LinkOutcome};
use serde::Serialize;

use super::loaders::{ids, lookup_ids};
use super::{respond, review, review_request};
use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader};
use crate::routes::projects::PREFIX;
use crate::routes::write::{bounded, json};

/// The path after `{iid}` of [`LINKS`].
pub const LINKS_SUFFIX: &str = "/work-items";
/// `GET` a merge request's work item links.
pub const LINKS: &str = "/api/v1/projects/{id}/merge-requests/{iid}/work-items";
/// `PUT` / `DELETE` one work item link.
pub const LINK: &str = "/api/v1/projects/{id}/merge-requests/{iid}/work-items/{ref}";

/// A work item as kanflow names it (02 §5.7's `work_items` entry).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkItemBody {
    /// `kanflow`.
    pub provider: String,
    /// The external (kanflow) project of the binding.
    pub project_id: String,
    /// `PBI-<n>`.
    #[serde(rename = "ref")]
    #[schema(rename = "ref")]
    pub reference: String,
}

fn item_body(w: &WorkItem) -> WorkItemBody {
    WorkItemBody {
        provider: w.kind.as_str().to_owned(),
        project_id: w.external_project_id.clone(), // clone: the body owns its values
        reference: w.reference.as_str().to_owned(),
    }
}

/// One link, open or ended.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkItemLinkBody {
    #[schema(minimum = 1)]
    pub link_id: i64,
    #[schema(minimum = 1)]
    pub work_item_id: i64,
    pub work_item: WorkItemBody,
    /// `auto` (detected in the title or the source branch) or `manual`.
    pub source: String,
    /// The person who linked it (`null` for a detected link).
    #[schema(required = true)]
    pub linked_by: Option<i64>,
    /// UTC, RFC 3339.
    pub linked_at: String,
    /// `null` while the link is open.
    #[schema(required = true)]
    pub unlinked_at: Option<String>,
    /// `auto` (the title no longer names it) or `manual`; `null` while open.
    #[schema(required = true)]
    pub unlink_source: Option<String>,
    #[schema(required = true)]
    pub unlinked_by: Option<i64>,
}

/// `GET …/work-items`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkItemLinksBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// Oldest first, at most 500.
    pub links: Vec<WorkItemLinkBody>,
    /// More than 500 link rows exist.
    pub truncated: bool,
}

/// `PUT …/work-items/{ref}`: what the link did.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkItemLinkAnswerBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// `linked` or `unchanged` (already linked, by detection or a person).
    pub outcome: String,
    #[schema(minimum = 1)]
    pub link_id: i64,
    #[schema(minimum = 1)]
    pub work_item_id: i64,
    pub work_item: WorkItemBody,
    /// `auto` or `manual`: how the (found) link was made.
    pub source: String,
}

fn answer_body(iid: i64, outcome: &LinkOutcome, a: &LinkAnswer) -> WorkItemLinkAnswerBody {
    WorkItemLinkAnswerBody {
        iid,
        outcome: match outcome {
            LinkOutcome::Changed => "linked",
            LinkOutcome::Unchanged => "unchanged",
        }
        .to_owned(),
        link_id: a.link,
        work_item_id: a.work_item.id,
        work_item: item_body(&a.work_item),
        source: a.source.as_str().to_owned(),
    }
}

/// The ids and the reference of
/// `/api/v1/projects/{id}/merge-requests/{iid}/work-items/{ref}` (raw path).
fn link_ids(raw_path: &str) -> Result<(ProjectId, i64, WorkItemRef), ForgeError> {
    let (id, rest) = raw_path
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once("/merge-requests/"))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(id).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let (iid, reference) = rest
        .split_once("/work-items/")
        .ok_or_else(|| param_invalid("iid", "not_positive_decimal"))?;
    let iid = parse_positive_id(iid)
        .filter(|_| !iid.contains('/'))
        .ok_or_else(|| param_invalid("iid", "not_positive_decimal"))?;
    let reference = WorkItemRef::parse(reference).ok_or_else(|| param_invalid("ref", "format"))?;
    Ok((ProjectId::new(id), iid, reference))
}

/// `GET …/work-items`: the merge request.
pub struct LinksOf;

impl Loader<AppState> for LinksOf {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let ids = ids(parts.uri.path(), LINKS_SUFFIX)?;
            lookup_ids(state, ids, viewer, MergeRequestEdit::default()).await
        })
    }
}

/// `PUT|DELETE …/work-items/{ref}`: the merge request (the reference is
/// checked first).
pub struct LinkOf;

impl Loader<AppState> for LinkOf {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let (project, iid, _) = link_ids(parts.uri.path())?;
            lookup_ids(state, (project, iid), viewer, MergeRequestEdit::default()).await
        })
    }
}

fn proof_iid(proof: &forge_auth::Authorized<MergeRequestCtx>) -> Result<i64, ForgeError> {
    proof.id().1.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid")
    })
}

/// `GET …/work-items` → `200` the links.
pub async fn links(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let (project, iid) = (w.proof.id().0, proof_iid(&w.proof)?);
        let review = review(&state)?;
        let stored = bounded(
            &state,
            "merge_request_load",
            review.merge_request(project, iid),
        )
        .await?
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "merge_request")
                .with_context("reason", "absent")
        })?;
        let listed = bounded(
            &state,
            "work_item_links_load",
            review.work_item_links(stored.id),
        )
        .await?;
        json(WorkItemLinksBody {
            iid,
            truncated: listed.truncated,
            links: listed
                .links
                .iter()
                .map(|l| WorkItemLinkBody {
                    link_id: l.id,
                    work_item_id: l.work_item.id,
                    work_item: item_body(&l.work_item),
                    source: l.source.as_str().to_owned(),
                    linked_by: l.linked_by.map(UserId::get),
                    linked_at: l.linked_at.clone(), // clone: the body owns its values
                    unlinked_at: l.unlinked_at.clone(),
                    unlink_source: l.unlink_source.map(|s| s.as_str().to_owned()),
                    unlinked_by: l.unlinked_by.map(UserId::get),
                })
                .collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `PUT …/work-items/{ref}` → `200` what the link did.
pub async fn link(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let iid = proof_iid(&w.proof)?;
        let (_, _, reference) = link_ids(&path)?;
        let (outcome, answer) = review(&state)?
            .link_work_item(&review_request(&w), &w.proof, &reference)
            .await?;
        json(answer_body(iid, &outcome, &answer))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `DELETE …/work-items/{ref}` → `204`.
pub async fn unlink(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let done = async {
        let (_, _, reference) = link_ids(&path)?;
        review(&state)?
            .unlink_work_item(&review_request(&w), &w.proof, &reference)
            .await
    }
    .await;
    match done {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_link_path_names_a_canonical_reference() {
        let (p, iid, r) =
            link_ids("/api/v1/projects/3/merge-requests/7/work-items/PBI-43").expect("ids");
        assert_eq!((p.get(), iid, r.as_str()), (3, 7, "PBI-43"));
        for bad in [
            "/api/v1/projects/3/merge-requests/7/work-items/pbi-43",
            "/api/v1/projects/3/merge-requests/7/work-items/PBI-43/x",
            "/api/v1/projects/3/merge-requests/7/work-items/",
            "/api/v1/projects/3/merge-requests/x/work-items/PBI-1",
        ] {
            assert!(link_ids(bad).is_err(), "{bad}");
        }
    }
}
