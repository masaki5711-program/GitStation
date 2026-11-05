//! The development status of an integration's work items (M3 I1d; plan 02
//! §5.2, §8.2, §11.2; ADR-0016 Amendment I1d; docs/api/dev-status.md,
//! `schema_version` 1, fields only ever added):
//!
//! - `GET /api/v1/integrations/{id}/work-items/{ref}/dev-status?project_id=…&state=…`
//!   (`IntegrationDevStatusRead`): one work item `PBI-<n>` on the bound
//!   kanflow projects (or the one `project_id` names) — its linked merge
//!   requests across the integration's bound projects.
//! - `GET /api/v1/integrations/{id}/dev-status?cursor=…&limit=…&state=…`
//!   (`IntegrationDevStatusRead`): every work item, page by page (the
//!   resync after a 410: `…/events/cursor` first, then these pages, then
//!   the feed from that cursor — or from `feed_cursor`).
//!
//! The principal rules are the feed's (`forge_auth::IntegrationFeedCtx`,
//! the same loader facts): only the integration's own service account with
//! `read_api`; other integrations, people (administrators included), bots
//! and a missing integration are the same 404; an agent's token,
//! `Forge-Act-As` and a disabled integration are 403. What it sees is the
//! feed's visible set (`prove_visible` inside forge-integ). A reference of
//! no bound, visible kanflow project is `FG-REPO-0005`.
//!
//! Parameters, strictly (`FG-WEB-0006 reason=param_invalid`): `{ref}` is
//! `PBI-<n>` in its canonical form (`param=ref rule=format`), `project_id`
//! a kanflow project id (`A-Z a-z 0-9 . _ : -`, 1–100), `state` one of
//! `all` (default) / `open` / `merged` / `closed`, `limit` 1..=100
//! (default 50), `cursor` the previous page's `next_cursor`; anything
//! else, repeated or empty is refused. The cursor's value is never logged.

use std::time::Duration;

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::IntegrationFeedCtx;
use forge_core::{parse_positive_id, IntegrationId, UserId, WorkItemRef};
use forge_errors::{ErrorCode, ForgeError};
use forge_integ::{
    checks_state, DevMergeRequest, DevStatus, Feed, MrStateFilter, PageQuery, WorkItemQuery,
    WorkItemStatus, DEFAULT_DEV_STATUS_LIMIT, MAX_DEV_STATUS_LIMIT,
};
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, QueryParam};
use crate::routes::integration_feed::ACT_AS_HEADER;
use crate::routes::protected_refs::found;
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` one work item's development status.
pub const WORK_ITEM: &str = "/api/v1/integrations/{id}/work-items/{ref}/dev-status";
/// `GET` every work item's development status, paged.
pub const ALL: &str = "/api/v1/integrations/{id}/dev-status";
const PREFIX: &str = "/api/v1/integrations/";
/// The format version of the answers.
pub const SCHEMA_VERSION: u32 = 1;
/// The whole read's bound: `FG-WEB-0002 operation=integration_dev_status`.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The query parameters of one work item (`openapi.json`).
pub const WORK_ITEM_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "project_id",
        description: "The kanflow project id of the binding (A-Z a-z 0-9 . _ : -, 1-100); without it every bound kanflow project's work item of that reference",
        integer_range: None,
    },
    QueryParam {
        name: "state",
        description: "all (default), open, merged or closed: which linked merge requests to list",
        integer_range: None,
    },
];

/// The query parameters of the paged status (`openapi.json`).
pub const ALL_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "cursor",
        description: "The previous page's next_cursor (opaque; bound to this integration). Without it the first page, as of now",
        integer_range: None,
    },
    QueryParam {
        name: "limit",
        description: "Work items per page, 1..=100 (default 50); a page also ends at about 1000 merge requests",
        integer_range: Some((1, 100)),
    },
    QueryParam {
        name: "state",
        description: "all (default), open, merged or closed: which linked merge requests to list",
        integer_range: None,
    },
];

/// The project of a merge request.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DevStatusProjectBody {
    #[schema(minimum = 1)]
    pub id: i64,
    /// The project's path now (display only; `null` if it cannot be read).
    #[schema(required = true)]
    pub path: Option<String>,
}

/// One check run on the merge request's head (M3 C1).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DevStatusCheckBody {
    /// `<namespace>/<name>`.
    pub name: String,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    /// `success`, `neutral`, `failure`, `error`, `action_required`; `null`
    /// until completed.
    #[schema(required = true)]
    pub conclusion: Option<String>,
}

/// The approvals given on the head and target now.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DevStatusApprovalsBody {
    /// Live approvals of the current head and target, counted or not (what
    /// a merge needs is the merge status: `GET …/merge-requests/{iid}/status`).
    pub given: u32,
}

/// How the merge request is linked to the work item.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DevStatusLinkBody {
    /// `auto` (detected in the title or the source branch) or `manual`.
    pub source: String,
    /// UTC, RFC 3339.
    pub linked_at: String,
}

/// One merge request linked to the work item.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DevStatusMergeRequestBody {
    pub project: DevStatusProjectBody,
    #[schema(minimum = 1)]
    pub iid: i64,
    pub title: String,
    /// `open`, `merged` or `closed` (a merge request closed without merging
    /// stays linked, plan 02 §8.2).
    pub state: String,
    pub draft: bool,
    /// Without `refs/heads/`.
    pub source_branch: String,
    pub target_branch: String,
    /// `null` before the first version.
    #[schema(required = true)]
    pub head_sha: Option<String>,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub updated_at: String,
    /// When it was merged (`null` otherwise).
    #[schema(required = true)]
    pub merged_at: Option<String>,
    /// When it was closed without merging (`null` otherwise).
    #[schema(required = true)]
    pub closed_at: Option<String>,
    pub approvals: DevStatusApprovalsBody,
    /// Every check run reported on the head (empty: none — never success).
    pub checks: Vec<DevStatusCheckBody>,
    /// `none` (no run), `pending`, `failure`, `neutral` or `success`.
    pub checks_state: String,
    pub link: DevStatusLinkBody,
}

/// One work item's development status.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DevStatusWorkItemBody {
    /// `kanflow`.
    pub provider: String,
    /// The kanflow project id of the binding.
    pub project_id: String,
    /// `PBI-<n>`.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The merge requests with an open link, newest first (at most 100).
    pub merge_requests: Vec<DevStatusMergeRequestBody>,
    /// More merge requests are linked than listed.
    pub merge_requests_truncated: bool,
}

/// One work item's answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DevStatusBody {
    /// `1`.
    pub schema_version: u32,
    /// Every event with `seq <= as_of_seq` is reflected here.
    pub as_of_seq: i64,
    /// The feed's cursor at `as_of_seq`: reading `GET …/events` from it
    /// misses nothing this answer does not show.
    pub feed_cursor: String,
    /// One per bound kanflow project (one with `project_id`).
    pub work_items: Vec<DevStatusWorkItemBody>,
}

/// One page of every work item's status.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DevStatusPageBody {
    /// `1`.
    pub schema_version: u32,
    /// The first page's position, on every page: every event with `seq <=
    /// as_of_seq` is reflected in the whole walk.
    pub as_of_seq: i64,
    /// The feed's cursor at `as_of_seq` (resume the feed from it after the
    /// last page).
    pub feed_cursor: String,
    /// In id order.
    pub work_items: Vec<DevStatusWorkItemBody>,
    /// The next page (`null`: this is the last).
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

fn merge_request_body(mr: DevMergeRequest) -> DevStatusMergeRequestBody {
    let checks_state = checks_state(&mr.checks).to_owned();
    DevStatusMergeRequestBody {
        project: DevStatusProjectBody {
            id: mr.project_id.get(),
            path: mr.project_path,
        },
        iid: mr.iid,
        title: mr.title,
        state: mr.state,
        draft: mr.draft,
        source_branch: mr.source_branch,
        target_branch: mr.target_branch,
        head_sha: mr.head_sha,
        created_at: mr.created_at,
        updated_at: mr.updated_at,
        merged_at: mr.merged_at,
        closed_at: mr.closed_at,
        approvals: DevStatusApprovalsBody {
            given: mr.approvals_given,
        },
        checks: mr
            .checks
            .into_iter()
            .map(|c| DevStatusCheckBody {
                name: c.name,
                status: c.status.as_str().to_owned(),
                conclusion: c.conclusion.map(|k| k.as_str().to_owned()),
            })
            .collect(),
        checks_state,
        link: DevStatusLinkBody {
            source: mr.link_source,
            linked_at: mr.linked_at,
        },
    }
}

fn work_item_body(item: WorkItemStatus) -> DevStatusWorkItemBody {
    DevStatusWorkItemBody {
        provider: item.provider,
        project_id: item.project_id,
        reference: item.reference,
        merge_requests: item
            .merge_requests
            .into_iter()
            .map(merge_request_body)
            .collect(),
        merge_requests_truncated: item.truncated,
    }
}

fn feed(state: &AppState) -> Result<&Feed, ForgeError> {
    state.feed.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "feed_unconfigured")
    })
}

/// The `{id}` (and `{ref}`) of a development-status path.
fn ids(path: &str, single: bool) -> Result<(IntegrationId, Option<&str>), ForgeError> {
    let (raw, rest) = path
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once('/'))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(raw).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let reference = if single {
        let reference = rest
            .strip_prefix("work-items/")
            .and_then(|r| r.strip_suffix("/dev-status"))
            .filter(|r| !r.is_empty() && !r.contains('/'))
            .ok_or_else(|| param_invalid("ref", "format"))?;
        Some(reference)
    } else {
        if rest != "dev-status" {
            return Err(param_invalid("id", "not_positive_decimal"));
        }
        None
    };
    Ok((IntegrationId::new(id), reference))
}

/// The development-status routes' loader: the feed's facts (the
/// integration of the path, the caller, `Forge-Act-As`). `{ref}` is
/// checked by the handler, after the decision (a malformed reference does
/// not tell another caller whether the integration exists).
pub struct DevStatusOf {
    single: bool,
}

/// The loader of `GET …/work-items/{ref}/dev-status`.
pub const DEV_STATUS_WORK_ITEM: DevStatusOf = DevStatusOf { single: true };
/// The loader of `GET …/dev-status`.
pub const DEV_STATUS_ALL: DevStatusOf = DevStatusOf { single: false };

impl Loader<AppState> for DevStatusOf {
    type Ctx = IntegrationFeedCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, IntegrationFeedCtx> {
        Box::pin(async move {
            let (id, _) = ids(parts.uri.path(), self.single)?;
            let act_as = parts.headers.contains_key(ACT_AS_HEADER);
            let loaded = bounded(
                state,
                "integration_dev_status_load",
                feed(state)?.target(id),
            )
            .await?;
            Ok(match loaded {
                Some(loaded) => found(IntegrationFeedCtx::new(loaded, viewer, act_as)),
                None => absent(),
            })
        })
    }
}

/// `key=value` pairs, each key at most once, only `allowed` keys.
fn pairs<'q>(
    query: Option<&'q str>,
    allowed: &[&'static str],
) -> Result<Vec<(&'static str, &'q str)>, ForgeError> {
    let mut out: Vec<(&'static str, &'q str)> = Vec::new();
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = allowed
            .iter()
            .copied()
            .find(|k| *k == key)
            .ok_or_else(|| param_invalid("query", "unknown_parameter"))?;
        if out.iter().any(|(k, _)| *k == key) {
            return Err(param_invalid(key, "repeated"));
        }
        if value.is_empty() {
            return Err(param_invalid(key, "empty"));
        }
        out.push((key, value));
    }
    Ok(out)
}

fn state_of(raw: Option<&str>) -> Result<MrStateFilter, ForgeError> {
    match raw {
        None => Ok(MrStateFilter::All),
        Some(raw) => MrStateFilter::parse(raw).ok_or_else(|| param_invalid("state", "unknown")),
    }
}

fn external_project_ok(raw: &str) -> bool {
    (1..=100).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn parse_work_item(path: &str, query: Option<&str>) -> Result<WorkItemQuery, ForgeError> {
    let (_, raw) = ids(path, true)?;
    let reference = raw
        .and_then(WorkItemRef::parse)
        .filter(|r| Some(r.as_str()) == raw)
        .ok_or_else(|| param_invalid("ref", "format"))?;
    let params = pairs(query, &["project_id", "state"])?;
    let get = |k: &str| params.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);
    let project_id = match get("project_id") {
        None => None,
        Some(raw) if external_project_ok(raw) => Some(raw.to_owned()),
        Some(_) => return Err(param_invalid("project_id", "format")),
    };
    Ok(WorkItemQuery {
        reference,
        project_id,
        state: state_of(get("state"))?,
    })
}

fn parse_page(query: Option<&str>) -> Result<PageQuery, ForgeError> {
    let params = pairs(query, &["cursor", "limit", "state"])?;
    let get = |k: &str| params.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);
    let limit = match get("limit") {
        None => DEFAULT_DEV_STATUS_LIMIT,
        Some(raw) => parse_positive_id(raw)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= MAX_DEV_STATUS_LIMIT)
            .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
    };
    Ok(PageQuery {
        cursor: get("cursor").map(str::to_owned),
        limit,
        state: state_of(get("state"))?,
    })
}

async fn timed<T>(
    read: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    match tokio::time::timeout(READ_TIMEOUT, read).await {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", "integration_dev_status")
            .with_context("timeout_ms", READ_TIMEOUT.as_millis())),
    }
}

/// `GET …/work-items/{ref}/dev-status` → `200`.
pub async fn work_item(state: AppState, request: Dispatched<IntegrationFeedCtx>) -> Response {
    let query = parse_work_item(request.raw_path(), request.raw_query());
    let w = request.into_write();
    let built = async {
        let query = query?;
        let status: DevStatus = timed(feed(&state)?.work_item_dev_status(&w.proof, &query)).await?;
        json(DevStatusBody {
            schema_version: SCHEMA_VERSION,
            as_of_seq: status.as_of_seq,
            feed_cursor: status.feed_cursor,
            work_items: status.work_items.into_iter().map(work_item_body).collect(),
        })
    }
    .await;
    match built {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `GET …/dev-status` → `200`.
pub async fn all(state: AppState, request: Dispatched<IntegrationFeedCtx>) -> Response {
    let query = parse_page(request.raw_query());
    let w = request.into_write();
    let built = async {
        let query = query?;
        let page: DevStatus = timed(feed(&state)?.dev_status_page(&w.proof, &query)).await?;
        json(DevStatusPageBody {
            schema_version: SCHEMA_VERSION,
            as_of_seq: page.as_of_seq,
            feed_cursor: page.feed_cursor,
            work_items: page.work_items.into_iter().map(work_item_body).collect(),
            next_cursor: page.next_cursor,
            has_more: page.has_more,
        })
    }
    .await;
    match built {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    fn rule(err: &ForgeError) -> String {
        err.context()
            .iter()
            .find(|(k, _)| *k == "rule")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    const ONE: &str = "/api/v1/integrations/7/work-items/PBI-12/dev-status";

    #[test]
    fn a_work_item_query_is_parsed_strictly() {
        let q = parse_work_item(ONE, None).expect("plain");
        assert_eq!(q.reference.as_str(), "PBI-12");
        assert_eq!((q.project_id, q.state), (None, MrStateFilter::All));
        let q = parse_work_item(ONE, Some("project_id=kf-1&state=closed")).expect("both");
        assert_eq!(q.project_id.as_deref(), Some("kf-1"));
        assert_eq!(q.state, MrStateFilter::Closed);
        for (path, query, wanted) in [
            (
                "/api/v1/integrations/7/work-items/PBI-012/dev-status",
                None,
                "format",
            ),
            (
                "/api/v1/integrations/7/work-items/pbi-12/dev-status",
                None,
                "format",
            ),
            (
                "/api/v1/integrations/7/work-items/PBI-12%2F1/dev-status",
                None,
                "format",
            ),
            (ONE, Some("state=draft"), "unknown"),
            (ONE, Some("state=open&state=open"), "repeated"),
            (ONE, Some("project_id="), "empty"),
            (ONE, Some("project_id=kf%201"), "format"),
            (ONE, Some("cursor=x"), "unknown_parameter"),
        ] {
            assert_eq!(
                rule(&parse_work_item(path, query).expect_err(path)),
                wanted,
                "{path} {query:?}"
            );
        }
    }

    #[test]
    fn a_page_query_is_parsed_strictly() {
        let q = parse_page(None).expect("none");
        assert_eq!((q.cursor, q.limit, q.state), (None, 50, MrStateFilter::All));
        let q = parse_page(Some("limit=100&cursor=abc&state=merged")).expect("all");
        assert_eq!(
            (q.cursor.as_deref(), q.limit, q.state),
            (Some("abc"), 100, MrStateFilter::Merged)
        );
        for (raw, wanted) in [
            ("limit=0", "out_of_range"),
            ("limit=101", "out_of_range"),
            ("cursor=", "empty"),
            ("after=1", "unknown_parameter"),
            ("limit=1&limit=2", "repeated"),
        ] {
            assert_eq!(
                rule(&parse_page(Some(raw)).expect_err(raw)),
                wanted,
                "{raw}"
            );
        }
    }

    /// The merge request's fields are exactly the golden example's
    /// (docs/api/dev-status.example.json; fields are only ever added).
    #[test]
    fn a_merge_request_has_the_golden_fields() {
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("../../../../docs/api/dev-status.example.json"))
                .expect("golden");
        let body = merge_request_body(DevMergeRequest {
            project_id: forge_core::ProjectId::new(7),
            project_path: Some("app/portal".to_owned()),
            iid: 88,
            title: "t".to_owned(),
            state: "open".to_owned(),
            draft: false,
            source_branch: "s".to_owned(),
            target_branch: "main".to_owned(),
            head_sha: None,
            created_at: "2026-11-04T01:00:00Z".to_owned(),
            updated_at: "2026-11-04T01:00:00Z".to_owned(),
            merged_at: None,
            closed_at: None,
            approvals_given: 0,
            checks: vec![forge_integ::DevCheckRun {
                name: "ci/gates".to_owned(),
                status: forge_integ::CheckStatus::Completed,
                conclusion: Some(forge_integ::CheckConclusion::Success),
            }],
            link_source: "auto".to_owned(),
            linked_at: "2026-11-04T01:00:01Z".to_owned(),
        });
        let got = serde_json::to_value(&body).expect("json");
        let keys = |v: &serde_json::Value| {
            let mut k: Vec<String> = v
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            k.sort();
            k
        };
        let want = &golden["work_item"]["work_items"][0]["merge_requests"][0];
        assert_eq!(keys(&got), keys(want));
        for nested in ["project", "approvals", "link"] {
            assert_eq!(keys(&got[nested]), keys(&want[nested]), "{nested}");
        }
        assert_eq!(keys(&got["checks"][0]), keys(&want["checks"][0]));
        assert_eq!(got["checks_state"], "success");
    }

    #[test]
    fn the_paths_are_checked() {
        assert_eq!(
            ids(ONE, true).expect("one"),
            (IntegrationId::new(7), Some("PBI-12"))
        );
        assert_eq!(
            ids("/api/v1/integrations/7/dev-status", false).expect("all"),
            (IntegrationId::new(7), None)
        );
        ids("/api/v1/integrations/07/dev-status", false).expect_err("leading zero");
        ids("/api/v1/integrations/7/events", false).expect_err("tail");
    }
}
