//! The loaders of the discussion routes (M3 R3): the ids of a thread's
//! path, a position's shape (`FG-WEB-0006` before any lookup) and the
//! contexts — the merge request for commenting, the project, merge request
//! and thread for resolving (any missing is the same `FG-REPO-0005`).

use axum::http::request::Parts;
use forge_auth::{DiscussionCtx, LoadedDiscussion, MergeRequestCtx, MergeRequestEdit};
use forge_core::{parse_positive_id, DiscussionId, DiscussionSide, ProjectId, UserId};
use forge_errors::ForgeError;
use forge_review::DiscussionPosition;

use super::discussions::{AddNote, PositionBody, StartDiscussion, DISCUSSIONS_SUFFIX};
use super::loaders::{found, lookup_ids, project_of};
use super::review;
use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, LoadFuture, Loader};
use crate::routes::projects::PREFIX;
use crate::routes::write::{absent, bounded};

/// A request's position, strictly (`FG-WEB-0006 param_invalid`).
pub(super) fn position_of(body: &PositionBody) -> Result<DiscussionPosition, ForgeError> {
    let side =
        DiscussionSide::parse(&body.side).ok_or_else(|| param_invalid("side", "unknown_value"))?;
    forge_review::check_position_shape(body.version, &body.path, body.line)?;
    Ok(DiscussionPosition {
        version: body.version,
        path: body.path.clone(), // clone: the body stays borrowed by the request
        side,
        line: body.line,
    })
}

/// The ids of `…/merge-requests/{iid}/discussions/{discussion_id}{suffix}`.
pub(super) fn discussion_ids(
    raw_path: &str,
    suffix: &str,
) -> Result<(ProjectId, i64, DiscussionId), ForgeError> {
    let (id, rest) = raw_path
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once("/merge-requests/"))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(id).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let (iid, tail) = rest
        .split_once("/discussions/")
        .ok_or_else(|| param_invalid("iid", "not_positive_decimal"))?;
    let iid = parse_positive_id(iid)
        .filter(|_| !iid.contains('/'))
        .ok_or_else(|| param_invalid("iid", "not_positive_decimal"))?;
    let discussion = tail
        .strip_suffix(suffix)
        .filter(|d| !d.contains('/'))
        .and_then(parse_positive_id)
        .ok_or_else(|| param_invalid("discussion_id", "not_positive_decimal"))?;
    Ok((ProjectId::new(id), iid, DiscussionId::new(discussion)))
}

/// `POST …/discussions`: the merge request (the position's shape is
/// checked first, before anything is looked up).
pub struct DiscussionToStart;

impl BodyLoader<AppState, StartDiscussion> for DiscussionToStart {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a StartDiscussion,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            if let Some(p) = &body.position {
                position_of(p)?;
            }
            let ids = super::loaders::ids(parts.uri.path(), DISCUSSIONS_SUFFIX)?;
            lookup_ids(state, ids, viewer, MergeRequestEdit::default()).await
        })
    }
}

/// `POST …/discussions/{discussion_id}/notes`: the merge request (the
/// discussion is checked by the command, after the decision).
pub struct DiscussionToReply;

impl BodyLoader<AppState, AddNote> for DiscussionToReply {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _body: &'a AddNote,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let (id, iid, _) = discussion_ids(parts.uri.path(), "/notes")?;
            lookup_ids(state, (id, iid), viewer, MergeRequestEdit::default()).await
        })
    }
}

/// `POST …/discussions/{discussion_id}/resolve` / `…/unresolve`: the
/// project, the merge request and the discussion; any missing is the same
/// `FG-REPO-0005`.
pub struct DiscussionToResolve(pub &'static str);

/// `…/resolve`.
pub const TO_RESOLVE: DiscussionToResolve = DiscussionToResolve("/resolve");
/// `…/unresolve`.
pub const TO_UNRESOLVE: DiscussionToResolve = DiscussionToResolve("/unresolve");

impl Loader<AppState> for DiscussionToResolve {
    type Ctx = DiscussionCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, DiscussionCtx> {
        Box::pin(async move {
            let (id, iid, discussion) = discussion_ids(parts.uri.path(), self.0)?;
            let Some((project, record)) = project_of(state, id, viewer).await? else {
                return Ok(absent());
            };
            let review = review(state)?;
            let Some(mr) =
                bounded(state, "merge_request_load", review.merge_request(id, iid)).await?
            else {
                return Ok(absent());
            };
            let Some(d) = bounded(
                state,
                "discussion_load",
                review.discussion(mr.id, discussion),
            )
            .await?
            else {
                return Ok(absent());
            };
            Ok(found(
                DiscussionCtx::new(
                    project,
                    forge_review::loaded_merge_request(&mr),
                    LoadedDiscussion {
                        id: d.id,
                        started_by: d.started_by,
                    },
                ),
                record,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_errors::ErrorCode;

    #[test]
    fn discussion_ids_are_strict_positive_decimals() {
        let path = "/api/v1/projects/4/merge-requests/9/discussions/31/resolve";
        let (p, iid, d) = discussion_ids(path, "/resolve").expect("ids");
        assert_eq!((p.get(), iid, d.get()), (4, 9, 31));
        for bad in [
            "/api/v1/projects/4/merge-requests/9/discussions/0/resolve",
            "/api/v1/projects/4/merge-requests/9/discussions/+1/resolve",
            "/api/v1/projects/4/merge-requests/9/discussions/1/2/resolve",
            "/api/v1/projects/4/merge-requests/x/discussions/1/resolve",
        ] {
            assert!(discussion_ids(bad, "/resolve").is_err(), "{bad}");
        }
    }

    #[test]
    fn a_position_side_is_old_or_new() {
        let body = |side: &str| PositionBody {
            version: 1,
            path: "lib.rs".to_owned(),
            side: side.to_owned(),
            line: 3,
        };
        assert_eq!(
            position_of(&body("new")).expect("new").side,
            DiscussionSide::New
        );
        let err = position_of(&body("right")).expect_err("unknown");
        assert_eq!(err.code(), ErrorCode::RequestInvalid);
    }
}
