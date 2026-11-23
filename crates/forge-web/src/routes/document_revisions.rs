//! Document revisions and edit claims (plan 13 §7.5, stage 4 part A,
//! ADR-0027), beside the document API (`documents`):
//!
//! | Route | Action | Answer |
//! |---|---|---|
//! | `GET …/documents/{doc_id}/status` | `RepoBrowse` | the open merge requests revising the document (every language; `holds`: the earliest per language holds it, a later one is a second revision) and its live edit claim |
//! | `GET …/documents/{doc_id}/versions` | `RepoBrowse` | the document's versions on the default branch: each applied merge that changed its blob is one version (derived from git and the merges; nothing written into a file) |
//! | `PUT …/documents/{doc_id}/edit-claim` | `DocumentEditClaim` | take or extend the caller's claim for 30 minutes (`FG-DOC-0007` when another person's is live) |
//! | `DELETE …/documents/{doc_id}/edit-claim` | `DocumentEditClaim` | release the caller's own claim (`FG-DOC-0007` for another's, 404 without one) |
//! | `POST …/documents/{doc_id}/edit-claim/revoke` | `DocumentEditClaimRevoke` | remove anybody's live claim (Maintainer; audited) |
//!
//! (`…` is `/api/v1/projects/{id}`.) A claim is keyed by the document's id
//! and is not checked against the tree (a document being written for the
//! first time may be claimed too). Versions read the default branch's
//! documents with the document API's bounds and rule for an agent
//! audience (a confidential document is answered as absent).
//!
//! **Versions**: the merges applied to the default branch (at most
//! [`MAX_VERSION_MERGES`], oldest first) are compared by the document's
//! blob before and after each (`git cat-file --batch-check`, one call); a
//! merge that changed it is the next version. The path is the document's
//! path at the tip: a file moved since is a new file to the merges before
//! the move. More merges than the bound: `incomplete {error_code:
//! FG-DOC-0002, reason: doc_versions}` and the numbers count the merges
//! read. TODO(plan 13 stage 4 part B): a merge marked "軽微な訂正" does not
//! raise the version, and the version tags `refs/tags/doc/<id>/r<N>`.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{Action, ProjectCtx, ResourceCtx};
use forge_checks::{ChecksRequest, DocRevisions, EditClaim};
use forge_core::{parse_positive_id, ObjectId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::RepoPath;
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::browse::{load_project_lookup, rev_body, RevBody, Target};
use crate::routes::context::audience;
use crate::routes::documents::{index_at, ref_id, tip_marks, visible_original, DocIncompleteBody};
use crate::routes::projects::PREFIX as PROJECT_PREFIX;
use crate::routes::specs::{caller_of, charge, respond, revision};
use crate::routes::write::{bounded, json, no_store};

/// `GET` a document's open revisions and edit claim.
pub const STATUS: &str = "/api/v1/projects/{id}/documents/{doc_id}/status";
/// `GET` a document's versions on the default branch.
pub const VERSIONS: &str = "/api/v1/projects/{id}/documents/{doc_id}/versions";
/// `PUT` / `DELETE` the caller's edit claim.
pub const CLAIM: &str = "/api/v1/projects/{id}/documents/{doc_id}/edit-claim";
/// `POST` revoke anybody's edit claim.
pub const REVOKE: &str = "/api/v1/projects/{id}/documents/{doc_id}/edit-claim/revoke";

/// The body cap of `PUT …/edit-claim`.
pub const MAX_BODY: usize = 1024;
/// Merges read for a document's versions at most.
pub const MAX_VERSION_MERGES: usize = 5_000;

const INFIX: &str = "/documents/";

/// `PUT …/edit-claim`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutEditClaim {
    /// The open merge request the editing is for (optional; kept when an
    /// extension names none).
    #[serde(default)]
    #[schema(minimum = 1)]
    pub merge_request_iid: Option<i64>,
}

/// A live edit claim.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EditClaimBody {
    pub doc_id: String,
    /// The person editing.
    #[schema(minimum = 1)]
    pub user_id: i64,
    /// The merge request the editing is for, if named.
    #[schema(required = true, minimum = 1)]
    pub merge_request_iid: Option<i64>,
    /// When it was taken (UTC, RFC 3339).
    pub claimed_at: String,
    /// When it lapses unless extended (UTC, RFC 3339; 30 minutes after
    /// it was taken or last extended).
    pub expires_at: String,
}

/// The answer of `PUT …/edit-claim`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EditClaimTakenBody {
    pub claim: EditClaimBody,
    /// The caller's own live claim was extended (else it was taken).
    pub extended: bool,
}

/// An open merge request revising the document.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentRevisionBody {
    /// The translation's language (`null`: the original).
    #[schema(required = true)]
    pub lang: Option<String>,
    #[schema(minimum = 1)]
    pub merge_request_iid: i64,
    pub title: String,
    #[schema(minimum = 1)]
    pub author_id: i64,
    /// When it first revised the document (UTC, RFC 3339).
    pub since: String,
    /// The head its documents were derived from.
    pub head_sha: String,
    /// The earliest open merge request of this language holds the
    /// revision; `false` marks a second revision (ADR-0027 §1).
    pub holds: bool,
}

/// What the caller could do now: trial `authorize()` decisions (hints for
/// the screen; the routes decide again).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentStatusViewer {
    /// `PUT|DELETE …/edit-claim`.
    pub can_claim: bool,
    /// `POST …/edit-claim/revoke`.
    pub can_revoke: bool,
}

/// `GET …/documents/{doc_id}/status`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentStatusBody {
    pub doc_id: String,
    /// By language, the earliest first.
    pub revisions: Vec<DocumentRevisionBody>,
    /// The live claim (`null`: nobody is editing).
    #[schema(required = true)]
    pub edit_claim: Option<EditClaimBody>,
    pub viewer: DocumentStatusViewer,
}

/// One version of a document.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentVersionBody {
    /// 1, 2, …
    #[schema(minimum = 1)]
    pub revision: i64,
    #[schema(minimum = 1)]
    pub merge_request_iid: i64,
    pub title: String,
    /// UTC, RFC 3339.
    pub merged_at: String,
    /// The branch's tip after the merge.
    pub commit: String,
    /// The document's blob after the merge (`null`: the merge removed it).
    #[schema(required = true)]
    pub blob: Option<String>,
}

/// `GET …/documents/{doc_id}/versions`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentVersionsBody {
    pub doc_id: String,
    /// The default branch's tip the document was found at.
    pub rev: RevBody,
    /// The document's path there.
    pub path: String,
    /// The latest version (`null`: no merge changed it yet).
    #[schema(required = true, minimum = 1)]
    pub current_revision: Option<i64>,
    /// Oldest first.
    pub versions: Vec<DocumentVersionBody>,
    /// A bound was hit (`doc_versions`: more merges than were read; or the
    /// document index's bounds).
    #[schema(required = true)]
    pub incomplete: Option<DocIncompleteBody>,
}

fn claim_body(c: EditClaim) -> EditClaimBody {
    EditClaimBody {
        doc_id: c.doc_id,
        user_id: c.holder.get(),
        merge_request_iid: c.merge_request_iid,
        claimed_at: c.claimed_at,
        expires_at: c.expires_at,
    }
}

fn revisions(state: &AppState) -> Result<&DocRevisions, ForgeError> {
    state.doc_revisions.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "checks_unconfigured")
    })
}

/// The project id before `/documents/` and the document id after it, the
/// rest of the path being `suffix`.
fn params(raw_path: &str, suffix: &str) -> Result<(ProjectId, String), ForgeError> {
    let bad_id = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PROJECT_PREFIX).ok_or_else(bad_id)?;
    let (project, tail) = rest.split_once(INFIX).ok_or_else(bad_id)?;
    if project.contains('/') {
        return Err(bad_id());
    }
    let project = parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad_id)?;
    let doc = tail
        .strip_suffix(suffix)
        .ok_or_else(|| param_invalid("doc_id", "invalid"))?;
    Ok((project, ref_id("doc_id", doc)?))
}

/// The routes' loader: the project before `/documents/` (the document id
/// is checked after the decision).
pub struct DocumentRevisionOf;

impl DocumentRevisionOf {
    async fn lookup(
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<ProjectCtx>, ForgeError> {
        let id = parts
            .uri
            .path()
            .strip_prefix(PROJECT_PREFIX)
            .and_then(|rest| rest.split_once(INFIX))
            .and_then(|(id, _)| (!id.contains('/')).then_some(id))
            .and_then(parse_positive_id)
            .map(ProjectId::new)
            .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
        load_project_lookup(state, id, viewer).await
    }
}

impl Loader<AppState> for DocumentRevisionOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(Self::lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, PutEditClaim> for DocumentRevisionOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a PutEditClaim,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(Self::lookup(state, parts, viewer))
    }
}

fn request_of<B>(w: &WriteParts<ProjectCtx, B>) -> ChecksRequest {
    ChecksRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

/// The path's ids, checked to name the proof's project.
fn ids<B>(
    w: &WriteParts<ProjectCtx, B>,
    raw_path: &str,
    suffix: &str,
) -> Result<String, ForgeError> {
    let (project, doc) = params(raw_path, suffix)?;
    if project != *w.proof.id() {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "loaded_another_project")
            .with_context("project_id", project.get()));
    }
    Ok(doc)
}

/// A trial decision of `action` on the loaded project (a hint).
fn may<B>(w: &WriteParts<ProjectCtx, B>, action: Action) -> bool {
    forge_auth::authorize(w.proof.principal(), action, &w.proof.ctx().resource()).is_ok()
}

/// For an AI agent audience, the document must be one the document API
/// would give it ([`visible_original`] at the default branch's tip:
/// confidential there, unknown, or no default branch → `FG-REPO-0005`, the
/// withheld event logged). A person is decided by the route alone.
async fn withheld_from_agent(
    state: &AppState,
    request: Dispatched<ProjectCtx>,
    raw_path: &str,
) -> (Dispatched<ProjectCtx>, Result<(), ForgeError>) {
    const ROUTE: &str = "documents_status";
    // The request is only borrowed outside the awaits (it is not `Sync`).
    let prepared = (|| {
        let who = audience(&request)?;
        if !who.is_agent() {
            return Ok(None);
        }
        let (_, id) = params(raw_path, "/status")?;
        let t = Target::of(state, &request)?;
        let caller = caller_of(&request)?;
        charge(state, caller, 1)?;
        Ok(Some((who, id, t, caller)))
    })();
    let (who, id, t, caller) = match prepared {
        Ok(Some(p)) => p,
        Ok(None) => return (request, Ok(())),
        Err(e) => return (request, Err(e)),
    };
    let read = async {
        let rev = match revision(&t, None).await {
            Ok(rev) => rev,
            Err(e) if e.code() == ErrorCode::RepositoryPathNotResolved => {
                return Err(ForgeError::new(ErrorCode::NotFound)
                    .with_context("reason", "doc_confidential")
                    .with_context("doc_id", id.clone()) // clone: the log's copy
                    .with_context("detail", "no_default_branch"));
            }
            Err(e) => return Err(e),
        };
        let index = index_at(&t, &rev).await?;
        let tip = tip_marks(state, caller, &t, who, &rev).await?;
        Ok((index, tip))
    }
    .await;
    let decided = match read {
        Ok((index, tip)) => {
            visible_original(&request, ROUTE, who, (&index, &tip), &id, None).map(drop)
        }
        Err(e) => Err(e),
    };
    (request, decided)
}

/// `GET …/documents/{doc_id}/status` → `200`.
pub async fn status(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let raw_path = request.raw_path().to_owned();
    // Review HIGH 3 (plan 06 D-5): an AI agent is answered a confidential
    // or unknown document as absent, before anything about it is read.
    let (request, withheld) = withheld_from_agent(&state, request, &raw_path).await;
    let w = request.into_write();
    let built = async {
        withheld?;
        let doc = ids(&w, &raw_path, "/status")?;
        let got = bounded(
            &state,
            "document_status_read",
            revisions(&state)?.status(&w.proof, &doc),
        )
        .await?;
        let mut seen: Vec<Option<String>> = Vec::new();
        let revisions = got
            .revisions
            .into_iter()
            .map(|r| {
                let holds = !seen.contains(&r.key.lang);
                if holds {
                    seen.push(r.key.lang.clone()); // clone: the marker keeps its language
                }
                DocumentRevisionBody {
                    lang: r.key.lang,
                    merge_request_iid: r.iid,
                    title: r.title,
                    author_id: r.author.get(),
                    since: r.since,
                    head_sha: r.head_sha,
                    holds,
                }
            })
            .collect();
        json(DocumentStatusBody {
            doc_id: doc,
            revisions,
            edit_claim: got.claim.map(claim_body),
            viewer: DocumentStatusViewer {
                can_claim: may(&w, Action::DocumentEditClaim),
                can_revoke: may(&w, Action::DocumentEditClaimRevoke),
            },
        })
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `PUT …/documents/{doc_id}/edit-claim` → `200`.
pub async fn take(state: AppState, request: Dispatched<ProjectCtx, PutEditClaim>) -> Response {
    let raw_path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let doc = ids(&w, &raw_path, "/edit-claim")?;
        if w.input.merge_request_iid.is_some_and(|i| i < 1) {
            return Err(param_invalid("merge_request_iid", "not_positive"));
        }
        let (outcome, claim) = revisions(&state)?
            .take(&request_of(&w), &w.proof, &doc, w.input.merge_request_iid)
            .await?;
        json(EditClaimTakenBody {
            claim: claim_body(claim),
            extended: outcome == forge_checks::ClaimOutcome::Extended,
        })
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `DELETE …/documents/{doc_id}/edit-claim` → `204`.
pub async fn release(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let raw_path = request.raw_path().to_owned();
    let w = request.into_write();
    let done = async {
        let doc = ids(&w, &raw_path, "/edit-claim")?;
        revisions(&state)?
            .release(&request_of(&w), &w.proof, &doc)
            .await
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `POST …/documents/{doc_id}/edit-claim/revoke` → `204`.
pub async fn revoke(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let raw_path = request.raw_path().to_owned();
    let w = request.into_write();
    let done = async {
        let doc = ids(&w, &raw_path, "/edit-claim/revoke")?;
        revisions(&state)?
            .revoke(&request_of(&w), &w.proof, &doc)
            .await
            .map(drop)
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// The versions of a document from the blob ids before and after each
/// merge (`blobs[2i]`, `blobs[2i + 1]`).
fn versions_of(
    merges: Vec<forge_checks::AppliedMerge>,
    blobs: &[Option<String>],
) -> Vec<DocumentVersionBody> {
    let mut out = Vec::new();
    let mut n = 0i64;
    for (i, m) in merges.into_iter().enumerate() {
        let before = blobs.get(2 * i).cloned().flatten(); // clone: compared and kept
        let after = blobs.get(2 * i + 1).cloned().flatten(); // clone: as above
        if before == after {
            continue;
        }
        n += 1;
        out.push(DocumentVersionBody {
            revision: n,
            merge_request_iid: m.iid,
            title: m.title,
            merged_at: m.merged_at,
            commit: m.after,
            blob: after,
        });
    }
    out
}

/// `GET …/documents/{doc_id}/versions` → `200`.
pub async fn versions(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    const ROUTE: &str = "documents_versions";
    respond(state, request, ROUTE, |state, request, t| async move {
        let (_, id) = params(request.raw_path(), "/versions")?;
        let who = audience(&request)?;
        let rev = revision(&t, None).await?;
        let index = index_at(&t, &rev).await?;
        let caller = caller_of(&request)?;
        let tip = tip_marks(&state, caller, &t, who, &rev).await?;
        let original = visible_original(&request, ROUTE, who, (&index, &tip), &id, None)?;
        let path = RepoPath::parse(&original.path).map_err(|rule| param_invalid("doc_id", rule))?;
        let refs = t.smart.browse.refs(t.repository.id).await?;
        let branch = refs
            .default_branch
            .as_deref()
            .map(|b| format!("refs/heads/{b}"))
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                    .with_context("reason", "ref_not_found")
            })?;
        let (merges, more) = revisions(&state)?
            .applied_merges(request.proof(), &branch, MAX_VERSION_MERGES)
            .await?;
        let oid = |raw: &str| {
            ObjectId::parse(raw).map_err(|_| {
                ForgeError::new(ErrorCode::Internal).with_context("reason", "merge_sha_invalid")
            })
        };
        let mut commits = Vec::with_capacity(merges.len() * 2);
        for m in &merges {
            commits.push(oid(&m.before)?);
            commits.push(oid(&m.after)?);
        }
        let blobs = t
            .smart
            .browse
            .blob_ids_at(t.repository.id, &commits, &path)
            .await?;
        let versions = versions_of(merges, &blobs);
        let incomplete = if more {
            Some(DocIncompleteBody {
                error_code: ErrorCode::DocIndexBounded.id().to_owned(),
                reason: "doc_versions".to_owned(),
            })
        } else {
            index.incomplete()
        };
        Ok(DocumentVersionsBody {
            doc_id: id,
            rev: rev_body(&rev),
            path: original.path.clone(), // clone: the answer owns its strings
            current_revision: versions.last().map(|v| v.revision),
            versions,
            incomplete,
        })
    })
    .await
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_path_names_the_project_and_the_document() {
        assert_eq!(
            params("/api/v1/projects/7/documents/QP-07-001/status", "/status").expect("one"),
            (ProjectId::new(7), "QP-07-001".to_owned())
        );
        assert_eq!(
            params(
                "/api/v1/projects/7/documents/QP-07-001/edit-claim/revoke",
                "/edit-claim/revoke"
            )
            .expect("revoke")
            .1,
            "QP-07-001"
        );
        for bad in [
            "/api/v1/projects/0/documents/QP/status",
            "/api/v1/projects/x/documents/QP/status",
            "/api/v1/projects/7/documents/QP/statusx",
            "/api/v1/projects/7/documents/Q%20P/status",
            "/api/v1/projects/7/documents//status",
        ] {
            assert!(params(bad, "/status").is_err(), "{bad}");
        }
    }

    #[test]
    fn a_merge_that_changed_the_blob_is_the_next_version() {
        let m = |iid: i64| forge_checks::AppliedMerge {
            iid,
            title: format!("mr {iid}"),
            before: "a".repeat(40),
            after: "b".repeat(40),
            merged_at: "2026-11-23T00:00:00.000000Z".into(),
        };
        let x = Some("1".repeat(40));
        let y = Some("2".repeat(40));
        // 1: created, 2: untouched, 3: changed, 4: removed.
        let blobs = vec![
            None,
            x.clone(),
            x.clone(),
            x.clone(),
            x.clone(),
            y.clone(),
            y.clone(),
            None,
        ];
        let v = versions_of(vec![m(1), m(2), m(3), m(4)], &blobs);
        let got: Vec<(i64, i64)> = v
            .iter()
            .map(|v| (v.revision, v.merge_request_iid))
            .collect();
        assert_eq!(got, vec![(1, 1), (2, 3), (3, 4)]);
        assert_eq!(v[2].blob, None);
        assert_eq!(v[1].blob, y);
    }
}
