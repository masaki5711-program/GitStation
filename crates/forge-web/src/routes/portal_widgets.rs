//! The PFS portal's home widgets (ADR-0036; the PFS portal contract v1,
//! `docs/portal/widgets.md`): the data of the three widgets GitStation
//! declares (`my-reviews`, `pipelines`, `stopped-count`) and the project
//! options of the first two.
//!
//! - Only the portal's credential reaches them (`crate::auth`: the portal's
//!   Bearer token, `azp = portal`, decided by `authorize()` as
//!   `PortalWidgetRead`); every project and merge request shown is decided
//!   again for the person (forge-review, forge-pipelines), so another
//!   person's or a private project's rows never appear.
//! - The query is the widget's settings as `key=value` plus `locale`
//!   (ja / en / id; `en` without one) and `tz` (an IANA zone name, checked;
//!   times are answered in UTC, which is tz-aware). Unknown keys are
//!   ignored (the contract); an invalid value or a repeated single-valued
//!   key is `422 FG-WEB-0011` with `param` and `rule`.
//! - The answer is the contract's envelope `{success, data}`; a failure is
//!   `{success: false, error: {code, message, request_id}}` with the same
//!   status ([`envelope_errors`]). `href`s are paths of this application.
//!   `Cache-Control: private, max-age=30` (≤ every widget's `refresh_sec`).

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, VARY};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::PortalWidgetCtx;
use forge_core::portal::{
    rfc3339_utc, valid_tz, Lamp, Tone, Word, CONTRACT_VERSION, MY_MERGE_REQUESTS_HREF,
    WIDGET_PREFIX,
};
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError, Lang};
use forge_pipelines::portal::Show;
use forge_review::{AttentionReason, UserListCount, UserListItem, WidgetList};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::write::{absent_because, bounded};

pub const MY_REVIEWS: &str = "/api/portal-widgets/v1/my-reviews";
pub const MY_REVIEWS_OPTIONS: &str = "/api/portal-widgets/v1/my-reviews/options/project";
pub const PIPELINES: &str = "/api/portal-widgets/v1/pipelines";
pub const PIPELINES_OPTIONS: &str = "/api/portal-widgets/v1/pipelines/options/project";
pub const STOPPED_COUNT: &str = "/api/portal-widgets/v1/stopped-count";

/// The most projects the `pipelines` setting may name (contract §3).
const MAX_PROJECTS: usize = 24;
/// `limit` of `my-reviews` (the declaration: 3..=10, default 5).
const LIMIT_RANGE: (usize, usize) = (3, 10);
const DEFAULT_LIMIT: usize = 5;
/// The contract's `stale_after_sec` of the widgets (refresh 60 / 120 s).
const STALE_LIST_S: u64 = 300;
const STALE_KPI_S: u64 = 600;

const LOCALE: QueryParam = QueryParam {
    name: "locale",
    description: "ja, en or id (default en): the language of every text",
    integer_range: None,
};
const TZ: QueryParam = QueryParam {
    name: "tz",
    description: "An IANA zone name (checked; times are answered in UTC)",
    integer_range: None,
};

pub const MY_REVIEWS_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "kind",
        description: "review (waiting for my review, default), mine (my open changes), stopped (stopped auto-merges I wrote or approved) or spec (specs awaiting my approval)",
        integer_range: None,
    },
    QueryParam {
        name: "project",
        description: "One project id (an option of options/project); none: every project the person may see",
        integer_range: Some((1, i64::MAX)),
    },
    QueryParam {
        name: "limit",
        description: "Items, 3..=10 (default 5)",
        integer_range: Some((3, 10)),
    },
    LOCALE,
    TZ,
];

pub const PIPELINES_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "project",
        description:
            "Project ids, repeated (at most 24); none: the projects the person holds a role on",
        integer_range: Some((1, i64::MAX)),
    },
    QueryParam {
        name: "show",
        description:
            "pipeline (the newest pipeline of main, default) or deploy (the newest deployment)",
        integer_range: None,
    },
    LOCALE,
    TZ,
];

pub const STOPPED_COUNT_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "scope",
        description: "mine (my changes, default) or reviews (also those I approved)",
        integer_range: None,
    },
    LOCALE,
    TZ,
];

/// Every widget route: the person's own widgets.
pub struct PortalWidgets;

impl Loader<AppState> for PortalWidgets {
    type Ctx = PortalWidgetCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, PortalWidgetCtx> {
        Box::pin(async move {
            // Only a person reaches a widget (the portal's credential has
            // one); anyone else is refused by `authorize()` on the
            // unresolved context.
            let Some(me) = viewer else {
                return Ok(absent_because("portal_widget_without_user"));
            };
            Ok(Lookup::Found {
                ctx: PortalWidgetCtx::new(me),
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

// ------------------------------------------------------------ the query

/// `FG-WEB-0011` for `param` with `rule`.
fn invalid(param: &'static str, rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::PortalWidgetSettingInvalid)
        .with_context("param", param)
        .with_context("rule", rule)
}

/// The query's pairs, percent-decoded; a key without `=` has an empty
/// value.
fn pairs(raw: Option<&str>) -> Vec<(String, String)> {
    url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

/// The one value of a single-valued `key` (repeated → `repeated`).
fn single<'a>(
    pairs: &'a [(String, String)],
    key: &'static str,
) -> Result<Option<&'a str>, ForgeError> {
    let mut found = pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.as_str());
    let first = found.next();
    if found.next().is_some() {
        return Err(invalid(key, "repeated"));
    }
    Ok(first)
}

/// `locale` and `tz` (every widget).
fn common(pairs: &[(String, String)]) -> Result<Lang, ForgeError> {
    let lang = match single(pairs, "locale")? {
        None => Lang::En,
        Some("ja") => Lang::Ja,
        Some("en") => Lang::En,
        Some("id") => Lang::Id,
        Some(_) => return Err(invalid("locale", "unknown_value")),
    };
    if let Some(tz) = single(pairs, "tz")? {
        if !valid_tz(tz) {
            return Err(invalid("tz", "invalid"));
        }
    }
    Ok(lang)
}

fn project_id(raw: &str) -> Result<ProjectId, ForgeError> {
    parse_positive_id(raw)
        .map(ProjectId::new)
        .ok_or_else(|| invalid("project", "not_positive_decimal"))
}

/// `my-reviews`' settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReviewsQuery {
    pub kind: WidgetList,
    pub project: Option<ProjectId>,
    pub limit: usize,
    pub lang: Lang,
}

pub(crate) fn reviews_query(raw: Option<&str>) -> Result<ReviewsQuery, ForgeError> {
    let pairs = pairs(raw);
    let lang = common(&pairs)?;
    let kind = match single(&pairs, "kind")? {
        None | Some("review") => WidgetList::Review,
        Some("mine") => WidgetList::Mine,
        Some("stopped") => WidgetList::StoppedInvolved,
        Some("spec") => WidgetList::Spec,
        Some(_) => return Err(invalid("kind", "unknown_value")),
    };
    let project = single(&pairs, "project")?.map(project_id).transpose()?;
    let limit = match single(&pairs, "limit")? {
        None => DEFAULT_LIMIT,
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|n| (LIMIT_RANGE.0..=LIMIT_RANGE.1).contains(n))
            .ok_or_else(|| invalid("limit", "out_of_range"))?,
    };
    Ok(ReviewsQuery {
        kind,
        project,
        limit,
        lang,
    })
}

/// `pipelines`' settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelinesQuery {
    pub projects: Option<Vec<ProjectId>>,
    pub show: Show,
    pub lang: Lang,
}

pub(crate) fn pipelines_query(raw: Option<&str>) -> Result<PipelinesQuery, ForgeError> {
    let pairs = pairs(raw);
    let lang = common(&pairs)?;
    let show = match single(&pairs, "show")? {
        None | Some("pipeline") => Show::Pipeline,
        Some("deploy") => Show::Deploy,
        Some(_) => return Err(invalid("show", "unknown_value")),
    };
    let mut projects = Vec::new();
    for (_, value) in pairs.iter().filter(|(k, _)| k == "project") {
        let id = project_id(value)?;
        if !projects.contains(&id) {
            projects.push(id);
        }
    }
    if projects.len() > MAX_PROJECTS {
        return Err(invalid("project", "too_many"));
    }
    Ok(PipelinesQuery {
        projects: (!projects.is_empty()).then_some(projects),
        show,
        lang,
    })
}

/// `stopped-count`'s settings: `reviews` also counts what the person
/// approved.
pub(crate) fn stopped_query(raw: Option<&str>) -> Result<(WidgetList, Lang), ForgeError> {
    let pairs = pairs(raw);
    let lang = common(&pairs)?;
    let scope = match single(&pairs, "scope")? {
        None | Some("mine") => WidgetList::StoppedMine,
        Some("reviews") => WidgetList::StoppedInvolved,
        Some(_) => return Err(invalid("scope", "unknown_value")),
    };
    Ok((scope, lang))
}

/// The options' `locale` (the only key they read).
fn options_query(raw: Option<&str>) -> Result<Lang, ForgeError> {
    common(&pairs(raw))
}

// ------------------------------------------------------------ the answer

fn now_secs() -> Result<u64, ForgeError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "clock_before_epoch")
        })
}

/// The contract's success envelope (§6.3).
fn envelope(
    kind: &'static str,
    title: &str,
    href: Option<String>,
    stale_after_sec: u64,
    body: Value,
) -> Result<Value, ForgeError> {
    let mut data = json!({
        "v": CONTRACT_VERSION,
        "type": kind,
        "generated_at": rfc3339_utc(now_secs()?),
        "stale_after_sec": stale_after_sec,
        "title": title,
        "body": body,
    });
    if let (Some(href), Some(map)) = (href, data.as_object_mut()) {
        map.insert("href".to_owned(), json!(href));
    }
    Ok(json!({ "success": true, "data": data }))
}

fn answer(ctx: &crate::Ctx, built: Result<Value, ForgeError>) -> Response {
    match built {
        Ok(body) => {
            let mut res = (StatusCode::OK, Json(body)).into_response();
            let headers = res.headers_mut();
            headers.insert(
                CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=30"),
            );
            headers.insert(VARY, HeaderValue::from_static("Authorization"));
            res
        }
        Err(e) => ctx.fail(e).into_response(),
    }
}

fn unconfigured(what: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", what)
}

/// A list item's badge (contract §6.3 `list`).
fn badge(kind: WidgetList, item: &UserListItem, lang: Lang) -> Value {
    let (word, tone) = match kind {
        WidgetList::Review => (Word::BadgeReview, Tone::Info),
        WidgetList::Spec => (Word::BadgeSpec, Tone::Info),
        WidgetList::StoppedMine | WidgetList::StoppedInvolved => (Word::BadgeStopped, Tone::Stop),
        WidgetList::Mine => {
            if item.reasons.contains(&AttentionReason::RequiredCheckFailed) {
                (Word::BadgeChecksFailed, Tone::Stop)
            } else if !item.reasons.is_empty() {
                (Word::BadgeConflict, Tone::Warn)
            } else if item.merge_request.draft {
                (Word::BadgeDraft, Tone::Neutral)
            } else {
                (Word::BadgeOpen, Tone::Neutral)
            }
        }
    };
    json!({ "label": word.in_lang(lang), "tone": tone.as_str() })
}

fn list_title(kind: WidgetList) -> Word {
    match kind {
        WidgetList::Review => Word::TitleReview,
        WidgetList::Spec => Word::TitleSpec,
        WidgetList::Mine => Word::TitleMine,
        WidgetList::StoppedMine | WidgetList::StoppedInvolved => Word::TitleStopped,
    }
}

/// The contract's `list` body (§6.3).
pub(crate) fn list_body(kind: WidgetList, items: &[UserListItem], total: i64, lang: Lang) -> Value {
    let items: Vec<Value> = items
        .iter()
        .map(|item| {
            let mr = &item.merge_request;
            json!({
                "id": format!("{}!{}", item.project_path, mr.iid),
                "label": mr.title,
                "sub": format!("{} !{}", item.project_path, mr.iid),
                "badge": badge(kind, item, lang),
                "href": forge_core::portal::merge_request_href(&item.project_path, mr.iid),
            })
        })
        .collect();
    json!({ "items": items, "total": total })
}

/// `GET my-reviews` → the `list` widget.
pub async fn my_reviews(state: AppState, request: Dispatched<PortalWidgetCtx>) -> Response {
    let query = reviews_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        let q = query?;
        let review = state
            .review
            .as_deref()
            .ok_or_else(|| unconfigured("review_unconfigured"))?;
        let page = bounded(
            &state,
            "portal_widget_my_reviews",
            review.portal_merge_requests(&w.proof, q.kind, q.project, q.limit),
        )
        .await?;
        envelope(
            "list",
            list_title(q.kind).in_lang(q.lang),
            Some(MY_MERGE_REQUESTS_HREF.to_owned()),
            STALE_LIST_S,
            list_body(q.kind, &page.items, page.total.count, q.lang),
        )
    }
    .await;
    answer(&w.ctx, built)
}

/// `GET pipelines` → the `status` widget.
pub async fn pipelines(state: AppState, request: Dispatched<PortalWidgetCtx>) -> Response {
    let query = pipelines_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        let q = query?;
        let pipelines = state
            .pipelines
            .as_deref()
            .ok_or_else(|| unconfigured("pipelines_unconfigured"))?;
        let lamps = bounded(
            &state,
            "portal_widget_pipelines",
            pipelines.portal_lamps(&w.proof, q.projects.as_deref(), q.show),
        )
        .await?;
        let items: Vec<Value> = lamps
            .iter()
            .map(|l| {
                let label = Word::of_lamp(l.lamp).in_lang(q.lang);
                let state_label = match &l.tier {
                    Some(tier) if l.lamp != Lamp::Unknown => format!("{label} ({tier})"),
                    _ => label.to_owned(),
                };
                let href = match q.show {
                    Show::Pipeline => forge_core::portal::pipelines_href(&l.path),
                    Show::Deploy => forge_core::portal::project_href(&l.path),
                };
                json!({
                    "key": l.project.get().to_string(),
                    "label": l.path,
                    "state": l.lamp.as_str(),
                    "state_label": state_label,
                    "href": href,
                })
            })
            .collect();
        let title = match q.show {
            Show::Pipeline => Word::TitlePipelines,
            Show::Deploy => Word::TitleDeploys,
        };
        envelope(
            "status",
            title.in_lang(q.lang),
            None,
            STALE_LIST_S,
            json!({ "items": items }),
        )
    }
    .await;
    answer(&w.ctx, built)
}

/// The contract's `kpi` body (§6.3): the count against its standard 0.
pub(crate) fn kpi_body(count: UserListCount, lang: Lang) -> Value {
    let unit = Word::UnitChanges.in_lang(lang);
    let shown = if count.capped || count.truncated {
        format!("{}+", count.count)
    } else {
        count.count.to_string()
    };
    let (delta, tone) = if count.count == 0 {
        (Word::DeltaAtStandard.in_lang(lang).to_owned(), Tone::Ok)
    } else {
        (
            Word::DeltaAboveStandard
                .in_lang(lang)
                .replace("{n}", &shown),
            Tone::Stop,
        )
    };
    json!({
        "value": count.count,
        "unit": unit,
        "display": format!("{shown} {unit}"),
        "reference": {
            "label": Word::ReferenceStandard.in_lang(lang),
            "value": 0,
            "display": format!("0 {unit}"),
        },
        "delta": { "display": delta, "tone": tone.as_str() },
    })
}

/// `GET stopped-count` → the `kpi` widget.
pub async fn stopped_count(state: AppState, request: Dispatched<PortalWidgetCtx>) -> Response {
    let query = stopped_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        let (scope, lang) = query?;
        let review = state
            .review
            .as_deref()
            .ok_or_else(|| unconfigured("review_unconfigured"))?;
        let page = bounded(
            &state,
            "portal_widget_stopped_count",
            review.portal_merge_requests(&w.proof, scope, None, 0),
        )
        .await?;
        let title = if matches!(scope, WidgetList::StoppedInvolved) {
            Word::TitleStoppedReviews
        } else {
            Word::TitleStoppedMine
        };
        envelope(
            "kpi",
            title.in_lang(lang),
            Some(MY_MERGE_REQUESTS_HREF.to_owned()),
            STALE_KPI_S,
            kpi_body(page.total, lang),
        )
    }
    .await;
    answer(&w.ctx, built)
}

/// `GET {widget}/options/project` → `{success, data: {options}}`.
pub async fn project_options(state: AppState, request: Dispatched<PortalWidgetCtx>) -> Response {
    let query = options_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        query?;
        let pipelines = state
            .pipelines
            .as_deref()
            .ok_or_else(|| unconfigured("pipelines_unconfigured"))?;
        let options = bounded(
            &state,
            "portal_widget_options",
            pipelines.portal_projects(&w.proof),
        )
        .await?;
        let options: Vec<Value> = options
            .iter()
            .map(|o| json!({ "value": o.project.get().to_string(), "label": o.path }))
            .collect();
        Ok(json!({ "success": true, "data": { "options": options } }))
    }
    .await;
    answer(&w.ctx, built)
}

// ------------------------------------------------------------ failures

/// The largest error body rewritten (the standard body is far smaller).
const MAX_ERROR_BODY: usize = 16 * 1024;

/// Middleware (`app::build`): a failure of a widget route answers the
/// contract's `{success: false, error: {code, message, request_id}}` with
/// the same status and headers (contract §6.3). The standard body's
/// `error_code` and `user_message` become `code` and `message`; nothing
/// else is added.
pub(crate) async fn envelope_errors(req: Request, next: Next) -> Response {
    let widget = req.uri().path().starts_with(WIDGET_PREFIX);
    let res = next.run(req).await;
    if !widget || res.status().is_success() {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_ERROR_BODY).await else {
        // Not readable as a small body: answered as it is, without one.
        return Response::from_parts(parts, Body::empty());
    };
    let Ok(standard) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let text = |key: &str| standard.get(key).cloned().unwrap_or(Value::Null);
    let wrapped = json!({
        "success": false,
        "error": {
            "code": text("error_code"),
            "message": text("user_message"),
            "request_id": text("request_id"),
        },
    });
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts
        .headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Response::from_parts(parts, Body::from(wrapped.to_string()))
}

#[cfg(test)]
#[path = "portal_widgets_tests.rs"]
mod tests;
