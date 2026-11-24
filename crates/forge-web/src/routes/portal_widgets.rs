//! The PFS portal's home widgets (ADR-0036; the PFS portal contract v1,
//! `docs/portal/widgets.md`, widget design P1): the data of the widgets
//! GitStation declares (`my-reviews`, `pipelines`, `stopped-count`, and
//! from P1 `attention-count`, `inbox`, `menu`, from P2a `merge-queue`,
//! `spec-status`) and the project options of those with a `project`
//! setting.
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
//!   `Cache-Control: private, max-age=30` (≤ every widget's `refresh_sec`);
//!   a failure is `Cache-Control: no-store`.
//! - One gate for all of them ([`WidgetGate`], review S3): the requests
//!   running at once and the requests per minute, counted before the token
//!   is verified and the person looked up (`FG-WEB-0010
//!   route=portal_widgets`, with `Retry-After`).

use std::time::{Duration, Instant};

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
    menu_links, rfc3339_utc, valid_tz, Lamp, MenuPurpose, Tone, Word, APPROVABLE_HREF,
    CONTRACT_VERSION, HOME_HREF, MEMBER_PROJECTS_HREF, MY_MERGE_REQUESTS_HREF,
    UNREAD_NOTIFICATIONS_HREF, WIDGET_PREFIX,
};
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError, Lang};
use forge_pipelines::portal::Show;
use forge_review::portal::{PortalQueue, PortalSpec, QueueStatus, SpecStatus, MAX_STATUS_COUNT};
use forge_review::{AttentionReason, UserListCount, WidgetItem, WidgetList, WidgetOrder};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::namespaces::CallerLimiter;
use crate::routes::specs::{InFlight, Running};
use crate::routes::write::absent_because;

pub const MY_REVIEWS: &str = "/api/portal-widgets/v1/my-reviews";
pub const MY_REVIEWS_OPTIONS: &str = "/api/portal-widgets/v1/my-reviews/options/project";
pub const PIPELINES: &str = "/api/portal-widgets/v1/pipelines";
pub const PIPELINES_OPTIONS: &str = "/api/portal-widgets/v1/pipelines/options/project";
pub const STOPPED_COUNT: &str = "/api/portal-widgets/v1/stopped-count";
pub const ATTENTION_COUNT: &str = "/api/portal-widgets/v1/attention-count";
pub const ATTENTION_COUNT_OPTIONS: &str = "/api/portal-widgets/v1/attention-count/options/project";
pub const INBOX: &str = "/api/portal-widgets/v1/inbox";
pub const MENU: &str = "/api/portal-widgets/v1/menu";
pub const MENU_OPTIONS: &str = "/api/portal-widgets/v1/menu/options/project";
pub const MERGE_QUEUE: &str = "/api/portal-widgets/v1/merge-queue";
pub const MERGE_QUEUE_OPTIONS: &str = "/api/portal-widgets/v1/merge-queue/options/project";
pub const SPEC_STATUS: &str = "/api/portal-widgets/v1/spec-status";
pub const SPEC_STATUS_OPTIONS: &str = "/api/portal-widgets/v1/spec-status/options/project";

/// The most projects the `pipelines` setting may name (contract §3).
const MAX_PROJECTS: usize = 24;
/// `limit` of `my-reviews` (the declaration: 3..=10, default 5).
const LIMIT_RANGE: (usize, usize) = (3, 10);
const DEFAULT_LIMIT: usize = 5;
/// The contract's `stale_after_sec` of the widgets (refresh 60 / 120 s).
const STALE_LIST_S: u64 = 300;
const STALE_KPI_S: u64 = 600;
/// The `menu` changes only with a release (refresh 3600 s).
const STALE_MENU_S: u64 = 7200;
/// `spec-status` refreshes every 300 s.
const STALE_SPEC_S: u64 = 900;

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
        description: "review (waiting for my review, default), mine (my open changes), stopped (stopped auto-merges I wrote or approved), spec (specs awaiting my approval), threads (my unresolved comments that got a reply), conflicts (my changes with a conflict, a likely one, or one not ruled out) or queue (my changes in the merge queue, and those it removed)",
        integer_range: None,
    },
    QueryParam {
        name: "sort",
        description: "oldest (what has waited longest first, default) or newest",
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
    QueryParam {
        name: "only_problems",
        description: "true: only the projects whose lamp is stop or warn (default false)",
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

pub const ATTENTION_COUNT_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "scope",
        description: "reviews (waiting for my review, default), stopped (my stopped changes), spec (specs awaiting my approval) or failed (projects whose main build or test failed); deploy is not available yet (422)",
        integer_range: None,
    },
    QueryParam {
        name: "project",
        description: "Project ids, repeated (at most 24); none: every project the person may see (failed: the projects they hold a role on)",
        integer_range: Some((1, i64::MAX)),
    },
    LOCALE,
    TZ,
];

pub const INBOX_QUERY: &[QueryParam] = &[LOCALE, TZ];

pub const MENU_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "purpose",
        description: "developer (default), reviewer, spec, operations or manager: whose links",
        integer_range: None,
    },
    QueryParam {
        name: "project",
        description: "One project id (an option of options/project): the project the project links open; none: they ask to choose a project",
        integer_range: Some((1, i64::MAX)),
    },
    LOCALE,
    TZ,
];

pub const MERGE_QUEUE_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "project",
        description:
            "Project ids, repeated (at most 24); none: the projects the person holds a role on",
        integer_range: Some((1, i64::MAX)),
    },
    LOCALE,
    TZ,
];

pub const SPEC_STATUS_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "project",
        description: "Project ids, repeated (at most 24); none: the projects the person holds a role on that released a specification",
        integer_range: Some((1, i64::MAX)),
    },
    QueryParam {
        name: "show",
        description: "approval (changes awaiting approval, default), undecided (required decisions still open) or lint (changes whose spec or document check found a problem)",
        integer_range: None,
    },
    LOCALE,
    TZ,
];

/// Widget requests running at once, all persons together: the caller is
/// one portal backend, so the gate is one for the routes, not per person
/// (the person is not known before the token is verified). Each request is
/// a token check, the person's lookup and a few indexed reads; 16 at once
/// is far above a portal polling for a few hundred open home pages and
/// well below `server.max_connections_per_peer`.
pub const WIDGET_MAX_IN_FLIGHT: usize = 16;
/// Widget requests per [`WIDGET_LIMIT_WINDOW`], all persons together:
/// 100 a second — about 2 000 open home pages with three widgets each
/// refreshing once a minute (the declared `refresh_sec` are 60 and 120).
pub const WIDGET_LIMIT: u32 = 6_000;
pub const WIDGET_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// The one caller the gate counts: the portal.
const PORTAL_CALLER: UserId = UserId::new(0);

/// The widget routes' gate (review S3): taken by `crate::auth` after the
/// request's shape is checked and before the token is verified or anything
/// is read from the database, and held until the answer is produced.
#[derive(Debug)]
pub struct WidgetGate {
    max_in_flight: usize,
    in_flight: InFlight,
    limiter: CallerLimiter,
}

impl WidgetGate {
    pub fn new(max_in_flight: usize, limit: u32, window: Duration) -> Self {
        Self {
            max_in_flight,
            in_flight: InFlight::new(max_in_flight),
            limiter: CallerLimiter::new("portal_widgets", limit, window),
        }
    }

    /// A place for one more widget request: `FG-WEB-0010
    /// route=portal_widgets reason=in_flight` (`Retry-After: 1`) with
    /// `max_in_flight` running, `FG-WEB-0010 route=portal_widgets` over the
    /// rate (`Retry-After`: the rest of the window). A request refused for
    /// the running ones does not count against the rate.
    pub fn enter(&self, now: Instant) -> Result<Running<'_>, ForgeError> {
        let Some(running) = self.in_flight.enter(PORTAL_CALLER) else {
            return Err(ForgeError::new(ErrorCode::RateLimited)
                .with_context("route", "portal_widgets")
                .with_context("reason", "in_flight")
                .with_context("limit", self.max_in_flight)
                .with_context(crate::error::RETRY_AFTER_CONTEXT, 1u64));
        };
        self.limiter
            .check(PORTAL_CALLER, now)
            .map_err(|retry| self.limiter.refused(retry))?;
        Ok(running)
    }
}

impl Default for WidgetGate {
    fn default() -> Self {
        Self::new(WIDGET_MAX_IN_FLIGHT, WIDGET_LIMIT, WIDGET_LIMIT_WINDOW)
    }
}

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

/// The most one widget's reads may take: the portal cuts a widget off at
/// 3 s (contract §6.5), so the reads stop well before it whatever
/// `server.lookup_timeout` says (the same `RequestTimeout` as every bounded
/// read, logged with the operation).
pub const WIDGET_READ_BUDGET: Duration = Duration::from_millis(2_500);

/// [`crate::routes::write::bounded`] under [`WIDGET_READ_BUDGET`].
async fn widget_bounded<T>(
    state: &AppState,
    operation: &'static str,
    read: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    bounded_by(widget_limit(state.lookup_timeout), operation, read).await
}

/// The limit of one widget's reads under `lookup_timeout`.
pub(crate) fn widget_limit(lookup_timeout: Duration) -> Duration {
    lookup_timeout.min(WIDGET_READ_BUDGET)
}

/// `read` within `limit`: `FG-WEB-0002` (`RequestTimeout`) with the operation
/// and the limit past it.
pub(crate) async fn bounded_by<T>(
    limit: Duration,
    operation: &'static str,
    read: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    match tokio::time::timeout(limit, read).await {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", operation)
            .with_context("timeout_ms", limit.as_millis())),
    }
}

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

/// `true` / `false` of a `toggle` (absent: `false`).
fn toggle(pairs: &[(String, String)], key: &'static str) -> Result<bool, ForgeError> {
    match single(pairs, key)? {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(invalid(key, "not_boolean")),
    }
}

/// The repeated `project` of a `multi` setting: distinct, at most
/// [`MAX_PROJECTS`] (`None`: not given).
fn multi_projects(pairs: &[(String, String)]) -> Result<Option<Vec<ProjectId>>, ForgeError> {
    let mut projects = Vec::new();
    for (_, value) in pairs.iter().filter(|(k, _)| k == "project") {
        let id = project_id(value)?;
        if !projects.contains(&id) {
            if projects.len() == MAX_PROJECTS {
                return Err(invalid("project", "too_many"));
            }
            projects.push(id);
        }
    }
    Ok((!projects.is_empty()).then_some(projects))
}

/// `my-reviews`' settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReviewsQuery {
    pub kind: WidgetList,
    pub project: Option<ProjectId>,
    pub limit: usize,
    pub order: WidgetOrder,
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
        Some("threads") => WidgetList::Threads,
        Some("conflicts") => WidgetList::Conflicts,
        Some("queue") => WidgetList::Queue,
        Some(_) => return Err(invalid("kind", "unknown_value")),
    };
    let order = match single(&pairs, "sort")? {
        None | Some("oldest") => WidgetOrder::Oldest,
        Some("newest") => WidgetOrder::Newest,
        Some(_) => return Err(invalid("sort", "unknown_value")),
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
        order,
        lang,
    })
}

/// `pipelines`' settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelinesQuery {
    pub projects: Option<Vec<ProjectId>>,
    pub show: Show,
    /// Only the lamps `stop` and `warn`.
    pub only_problems: bool,
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
    Ok(PipelinesQuery {
        projects: multi_projects(&pairs)?,
        show,
        only_problems: toggle(&pairs, "only_problems")?,
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

/// What `attention-count` counts (widget design §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttentionScope {
    /// The person's merge requests whose auto-merge stopped.
    Stopped,
    /// Waiting for the person's review.
    Reviews,
    /// Specifications awaiting the person's approval.
    Spec,
    /// Projects whose newest `main` pipeline failed.
    Failed,
}

/// `attention-count`'s settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttentionQuery {
    pub scope: AttentionScope,
    pub projects: Option<Vec<ProjectId>>,
    pub lang: Lang,
}

pub(crate) fn attention_query(raw: Option<&str>) -> Result<AttentionQuery, ForgeError> {
    let pairs = pairs(raw);
    let lang = common(&pairs)?;
    let scope = match single(&pairs, "scope")? {
        None | Some("reviews") => AttentionScope::Reviews,
        Some("stopped") => AttentionScope::Stopped,
        Some("spec") => AttentionScope::Spec,
        Some("failed") => AttentionScope::Failed,
        // The design's `deploy` needs the per-person approval query (P2).
        Some("deploy") => return Err(invalid("scope", "not_available")),
        Some(_) => return Err(invalid("scope", "unknown_value")),
    };
    Ok(AttentionQuery {
        scope,
        projects: multi_projects(&pairs)?,
        lang,
    })
}

/// `menu`'s settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MenuQuery {
    pub purpose: MenuPurpose,
    pub project: Option<ProjectId>,
    pub lang: Lang,
}

pub(crate) fn menu_query(raw: Option<&str>) -> Result<MenuQuery, ForgeError> {
    let pairs = pairs(raw);
    let lang = common(&pairs)?;
    let purpose = match single(&pairs, "purpose")? {
        None => MenuPurpose::Developer,
        Some(raw) => MenuPurpose::parse(raw).ok_or_else(|| invalid("purpose", "unknown_value"))?,
    };
    let project = single(&pairs, "project")?.map(project_id).transpose()?;
    Ok(MenuQuery {
        purpose,
        project,
        lang,
    })
}

/// `merge-queue`'s settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueueQuery {
    pub projects: Option<Vec<ProjectId>>,
    pub lang: Lang,
}

pub(crate) fn queue_query(raw: Option<&str>) -> Result<QueueQuery, ForgeError> {
    let pairs = pairs(raw);
    let lang = common(&pairs)?;
    Ok(QueueQuery {
        projects: multi_projects(&pairs)?,
        lang,
    })
}

/// What `spec-status` shows of each project (widget design §3.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpecShow {
    /// The changes awaiting approval.
    Approval,
    /// The required decisions still open.
    Undecided,
    /// The changes whose spec or document check found a problem.
    Lint,
}

/// `spec-status`' settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpecQuery {
    pub projects: Option<Vec<ProjectId>>,
    pub show: SpecShow,
    pub lang: Lang,
}

pub(crate) fn spec_query(raw: Option<&str>) -> Result<SpecQuery, ForgeError> {
    let pairs = pairs(raw);
    let lang = common(&pairs)?;
    let show = match single(&pairs, "show")? {
        None | Some("approval") => SpecShow::Approval,
        Some("undecided") => SpecShow::Undecided,
        Some("lint") => SpecShow::Lint,
        Some(_) => return Err(invalid("show", "unknown_value")),
    };
    Ok(SpecQuery {
        projects: multi_projects(&pairs)?,
        show,
        lang,
    })
}

/// The options' `locale` (the only key they read); also `inbox`'s
/// settings (it has none of its own).
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
fn badge(kind: WidgetList, row: &WidgetItem, lang: Lang) -> Value {
    let item = &row.item;
    let extra = &row.extra;
    let (word, tone) = match kind {
        WidgetList::Threads => (Word::BadgeReply, Tone::Info),
        WidgetList::Conflicts => {
            if item.reasons.contains(&AttentionReason::Conflict) {
                (Word::BadgeConflict, Tone::Stop)
            } else if item.reasons.contains(&AttentionReason::ConflictPredicted) {
                (Word::BadgeConflictPredicted, Tone::Warn)
            } else {
                // Not ruled out (`conflict_unknown`): never "no conflict".
                (Word::BadgeConflictUnknown, Tone::Neutral)
            }
        }
        WidgetList::Queue => match extra.queue_position {
            Some(n) if !extra.queue_ejected => {
                let label = Word::BadgeQueuePosition
                    .in_lang(lang)
                    .replace("{n}", &n.to_string());
                return json!({ "label": label, "tone": Tone::Info.as_str() });
            }
            _ => (Word::BadgeQueueRemoved, Tone::Stop),
        },
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
        WidgetList::Threads => Word::TitleThreads,
        WidgetList::Conflicts => Word::TitleConflicts,
        WidgetList::Queue => Word::TitleQueue,
    }
}

/// A list item's `sub` and `href`: the conflicts open the changes (with the
/// other merge request's number when a conflict with it is predicted), the
/// queue opens the queue of the target branch.
fn sub_and_href(kind: WidgetList, row: &WidgetItem) -> (String, String) {
    let item = &row.item;
    let mr = &item.merge_request;
    let path = item.project_path.as_str();
    let sub = format!("{path} !{}", mr.iid);
    match kind {
        WidgetList::Conflicts => {
            let sub = match row.extra.conflict_peer_iid {
                Some(peer) => format!("{sub} \u{2194} !{peer}"),
                None => sub,
            };
            (
                sub,
                forge_core::portal::merge_request_changes_href(path, mr.iid),
            )
        }
        WidgetList::Queue => (
            sub,
            forge_core::portal::merge_queue_href(path, mr.target_branch.as_str()),
        ),
        WidgetList::Review
        | WidgetList::Spec
        | WidgetList::Mine
        | WidgetList::StoppedMine
        | WidgetList::StoppedInvolved
        | WidgetList::Threads => (sub, forge_core::portal::merge_request_href(path, mr.iid)),
    }
}

/// The contract's `list` body (§6.3).
pub(crate) fn list_body(kind: WidgetList, items: &[WidgetItem], total: i64, lang: Lang) -> Value {
    let items: Vec<Value> = items
        .iter()
        .map(|row| {
            let mr = &row.item.merge_request;
            let (sub, href) = sub_and_href(kind, row);
            json!({
                "id": format!("{}!{}", row.item.project_path, mr.iid),
                "label": mr.title,
                "sub": sub,
                "badge": badge(kind, row, lang),
                "href": href,
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
        let page = widget_bounded(
            &state,
            "portal_widget_my_reviews",
            review.portal_merge_requests(
                &w.proof,
                q.kind,
                q.project.as_ref().map(std::slice::from_ref),
                q.order,
                q.limit,
            ),
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

/// Every lamp (`pipelines` without `only_problems`).
fn any_lamp(_: Lamp) -> bool {
    true
}

/// `stop` and `warn` (`pipelines?only_problems=true`).
fn is_problem(lamp: Lamp) -> bool {
    matches!(lamp, Lamp::Stop | Lamp::Warn)
}

/// `stop` (`attention-count?scope=failed`).
fn is_stop(lamp: Lamp) -> bool {
    lamp == Lamp::Stop
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
        let lamps = widget_bounded(
            &state,
            "portal_widget_pipelines",
            pipelines.portal_lamps(
                &w.proof,
                q.projects.as_deref(),
                q.show,
                if q.only_problems {
                    is_problem
                } else {
                    any_lamp
                },
            ),
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

/// The contract's `kpi` body (§6.3): the count of changes against its
/// standard 0 (`stop` above it).
pub(crate) fn kpi_body(count: UserListCount, lang: Lang) -> Value {
    kpi_body_of(count, Word::UnitChanges, Tone::Stop, lang)
}

/// The `kpi` body of `count` in `unit` against the standard 0: `ok` at it,
/// `above` over it. A capped or truncated count is shown `n+`.
pub(crate) fn kpi_body_of(count: UserListCount, unit: Word, above: Tone, lang: Lang) -> Value {
    let unit = unit.in_lang(lang);
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
            above,
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
        let page = widget_bounded(
            &state,
            "portal_widget_stopped_count",
            review.portal_merge_requests(&w.proof, scope, None, WidgetOrder::Oldest, 0),
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

/// `GET attention-count` → the `kpi` widget (widget design §3.3).
pub async fn attention_count(state: AppState, request: Dispatched<PortalWidgetCtx>) -> Response {
    let query = attention_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        let q = query?;
        let projects = q.projects.as_deref();
        let (title, href, body) = match q.scope {
            AttentionScope::Failed => {
                let pipelines = state
                    .pipelines
                    .as_deref()
                    .ok_or_else(|| unconfigured("pipelines_unconfigured"))?;
                let lamps = widget_bounded(
                    &state,
                    "portal_widget_attention_count",
                    pipelines.portal_lamps(&w.proof, projects, Show::Pipeline, is_stop),
                )
                .await?;
                // Only failed lamps are kept (before the cap of 24): 24 of
                // them means more may fail (said, never silently low).
                let count = UserListCount {
                    count: i64::try_from(lamps.len()).unwrap_or(i64::MAX),
                    capped: false,
                    truncated: lamps.len() >= MAX_PROJECTS,
                };
                (
                    Word::TitleFailed,
                    MEMBER_PROJECTS_HREF,
                    kpi_body_of(count, Word::UnitProjects, Tone::Stop, q.lang),
                )
            }
            scope @ (AttentionScope::Stopped | AttentionScope::Reviews | AttentionScope::Spec) => {
                let (list, title, href, above) = match scope {
                    AttentionScope::Stopped => (
                        WidgetList::StoppedMine,
                        Word::TitleStoppedMine,
                        MY_MERGE_REQUESTS_HREF,
                        Tone::Stop,
                    ),
                    AttentionScope::Spec => (
                        WidgetList::Spec,
                        Word::TitleSpec,
                        APPROVABLE_HREF,
                        Tone::Warn,
                    ),
                    AttentionScope::Reviews | AttentionScope::Failed => (
                        WidgetList::Review,
                        Word::TitleReview,
                        APPROVABLE_HREF,
                        Tone::Warn,
                    ),
                };
                let review = state
                    .review
                    .as_deref()
                    .ok_or_else(|| unconfigured("review_unconfigured"))?;
                let page = widget_bounded(
                    &state,
                    "portal_widget_attention_count",
                    review.portal_merge_requests(&w.proof, list, projects, WidgetOrder::Oldest, 0),
                )
                .await?;
                (
                    title,
                    href,
                    kpi_body_of(page.total, Word::UnitChanges, above, q.lang),
                )
            }
        };
        envelope(
            "kpi",
            title.in_lang(q.lang),
            Some(href.to_owned()),
            STALE_KPI_S,
            body,
        )
    }
    .await;
    answer(&w.ctx, built)
}

/// The `inbox` widget's `kpi` body: the unread count (no standard).
pub(crate) fn inbox_body(count: i64, capped: bool, lang: Lang) -> Value {
    let unit = Word::UnitUnread.in_lang(lang);
    let shown = if capped {
        format!("{count}+")
    } else {
        count.to_string()
    };
    json!({
        "value": count,
        "unit": unit,
        "display": format!("{shown} {unit}"),
    })
}

/// `GET inbox` → the `kpi` widget (widget design §3.8): the person's
/// unread notifications they may see now.
pub async fn inbox(state: AppState, request: Dispatched<PortalWidgetCtx>) -> Response {
    let query = options_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        let lang = query?;
        let inbox = state
            .inbox
            .as_deref()
            .ok_or_else(|| unconfigured("in_app_notifications_unconfigured"))?;
        let unread = widget_bounded(
            &state,
            "portal_widget_inbox",
            inbox.portal_unread_count(&w.proof),
        )
        .await?;
        envelope(
            "kpi",
            Word::TitleInbox.in_lang(lang),
            Some(UNREAD_NOTIFICATIONS_HREF.to_owned()),
            STALE_KPI_S,
            inbox_body(unread.count, unread.capped, lang),
        )
    }
    .await;
    answer(&w.ctx, built)
}

/// The `menu` widget's `links` body; `admin`: the administrator link last
/// (`forge_auth::portal_admin_menu`, ADR-0021 amendment 2026-11-24).
pub(crate) fn menu_body(
    purpose: MenuPurpose,
    project_path: Option<&str>,
    admin: bool,
    lang: Lang,
) -> Value {
    let mut links = menu_links(purpose, project_path);
    if admin {
        links.push(forge_core::portal::admin_menu_link());
    }
    let items: Vec<Value> = links
        .into_iter()
        .map(|l| json!({ "label": l.label.in_lang(lang), "href": l.href }))
        .collect();
    json!({ "items": items })
}

/// `GET menu` → the `links` widget (widget design §3.1, §4): the purpose's
/// links; a project's links open the chosen project when the person may
/// read it (otherwise, as without one, they ask to choose a project).
pub async fn menu(state: AppState, request: Dispatched<PortalWidgetCtx>) -> Response {
    let query = menu_query(request.raw_query());
    let w = request.into_write();
    // Decided by authorize() on the account's flag, never the token's roles.
    let admin = forge_auth::portal_admin_menu(&w.proof);
    let built = async {
        let q = query?;
        let path = match q.project {
            None => None,
            Some(id) => {
                let pipelines = state
                    .pipelines
                    .as_deref()
                    .ok_or_else(|| unconfigured("pipelines_unconfigured"))?;
                widget_bounded(
                    &state,
                    "portal_widget_menu",
                    pipelines.portal_project(&w.proof, id),
                )
                .await?
                .map(|o| o.path)
            }
        };
        let title = Word::TitleMenu
            .in_lang(q.lang)
            .replace("{purpose}", q.purpose.word().in_lang(q.lang));
        envelope(
            "links",
            &title,
            Some(HOME_HREF.to_owned()),
            STALE_MENU_S,
            menu_body(q.purpose, path.as_deref(), admin, q.lang),
        )
    }
    .await;
    answer(&w.ctx, built)
}

/// A count shown: `n+` when it reached the cap of the reads, or when
/// `partial` says more may exist.
fn shown_count(n: i64, partial: bool) -> String {
    if partial || n >= MAX_STATUS_COUNT {
        format!("{n}+")
    } else {
        n.to_string()
    }
}

/// `word` with `{n}` replaced.
fn with_n(word: Word, n: &str, lang: Lang) -> String {
    word.in_lang(lang).replace("{n}", n)
}

/// The lamp and label of one queue (widget design §3.6): a merge request
/// the queue removed → `stop`, paused → `warn`, entries waiting → `info`,
/// otherwise `ok` (running a batch, or nothing waiting).
pub(crate) fn queue_lamp(q: &QueueStatus, lang: Lang) -> (Lamp, String) {
    if q.ejected > 0 {
        return (
            Lamp::Stop,
            with_n(Word::QueueEjected, &shown_count(q.ejected, false), lang),
        );
    }
    let waiting = with_n(Word::QueueWaiting, &shown_count(q.queued, false), lang);
    if q.paused {
        let paused = Word::QueuePaused.in_lang(lang);
        return if q.queued > 0 {
            (Lamp::Warn, format!("{paused} ({waiting})"))
        } else {
            (Lamp::Warn, paused.to_owned())
        };
    }
    if q.queued > 0 {
        return (Lamp::Info, waiting);
    }
    if q.testing > 0 {
        return (Lamp::Ok, Word::QueueRunning.in_lang(lang).to_owned());
    }
    (Lamp::Ok, Word::QueueIdle.in_lang(lang).to_owned())
}

/// The `merge-queue` body: one lamp per queue; a queue of another branch
/// than `main` is labelled with it.
pub(crate) fn queue_body(queues: &[PortalQueue], lang: Lang) -> Value {
    let items: Vec<Value> = queues
        .iter()
        .map(|q| {
            let s = &q.status;
            let short = s
                .target_branch
                .strip_prefix("refs/heads/")
                .unwrap_or(&s.target_branch);
            let label = if short == "main" {
                q.path.clone()
            } else {
                format!("{} ({short})", q.path)
            };
            let (lamp, state_label) = queue_lamp(s, lang);
            json!({
                "key": format!("{}:{short}", s.project.get()),
                "label": label,
                "state": lamp.as_str(),
                "state_label": state_label,
                "href": forge_core::portal::merge_queue_href(&q.path, &s.target_branch),
            })
        })
        .collect();
    json!({ "items": items })
}

/// The lamp and label of one specification project (widget design §3.7):
/// `approval` with waiting changes → `warn`; `undecided` with open
/// decisions → `warn`; `lint` with a failed check → `stop`. A change not
/// checked yet (or whose check answered `error`) is never "nothing found":
/// `unknown` when nothing else is to be said.
pub(crate) fn spec_lamp(s: &SpecStatus, show: SpecShow, lang: Lang) -> (Lamp, String) {
    match show {
        SpecShow::Approval => {
            if s.awaiting > 0 {
                (
                    Lamp::Warn,
                    with_n(Word::SpecAwaiting, &shown_count(s.awaiting, false), lang),
                )
            } else {
                (Lamp::Ok, Word::SpecNoneAwaiting.in_lang(lang).to_owned())
            }
        }
        SpecShow::Undecided => {
            if s.undecided > 0 {
                let n = shown_count(s.undecided, s.undecided_partial);
                (Lamp::Warn, with_n(Word::SpecUndecided, &n, lang))
            } else if s.unchecked > 0 || s.undecided_partial {
                (Lamp::Unknown, Word::SpecChecking.in_lang(lang).to_owned())
            } else {
                (Lamp::Ok, Word::SpecNoneUndecided.in_lang(lang).to_owned())
            }
        }
        SpecShow::Lint => {
            if s.failing > 0 {
                (
                    Lamp::Stop,
                    with_n(Word::SpecFindings, &shown_count(s.failing, false), lang),
                )
            } else if s.unchecked > 0 {
                (Lamp::Unknown, Word::SpecChecking.in_lang(lang).to_owned())
            } else {
                (Lamp::Ok, Word::SpecNoFindings.in_lang(lang).to_owned())
            }
        }
    }
}

/// The `spec-status` body: one lamp per project; `undecided` opens the
/// documents, the others the open changes.
pub(crate) fn spec_body(specs: &[PortalSpec], show: SpecShow, lang: Lang) -> Value {
    let items: Vec<Value> = specs
        .iter()
        .map(|p| {
            let (lamp, state_label) = spec_lamp(&p.status, show, lang);
            let href = match show {
                SpecShow::Undecided => forge_core::portal::documents_href(&p.path),
                SpecShow::Approval | SpecShow::Lint => {
                    forge_core::portal::open_changes_href(&p.path)
                }
            };
            json!({
                "key": p.status.project.get().to_string(),
                "label": p.path,
                "state": lamp.as_str(),
                "state_label": state_label,
                "href": href,
            })
        })
        .collect();
    json!({ "items": items })
}

/// `GET merge-queue` → the `status` widget (widget design §3.6).
pub async fn merge_queue(state: AppState, request: Dispatched<PortalWidgetCtx>) -> Response {
    let query = queue_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        let q = query?;
        let review = state
            .review
            .as_deref()
            .ok_or_else(|| unconfigured("review_unconfigured"))?;
        let queues = widget_bounded(
            &state,
            "portal_widget_merge_queue",
            review.portal_merge_queues(&w.proof, q.projects.as_deref()),
        )
        .await?;
        envelope(
            "status",
            Word::TitleMergeQueue.in_lang(q.lang),
            None,
            STALE_LIST_S,
            queue_body(&queues, q.lang),
        )
    }
    .await;
    answer(&w.ctx, built)
}

/// `GET spec-status` → the `status` widget (widget design §3.7).
pub async fn spec_status(state: AppState, request: Dispatched<PortalWidgetCtx>) -> Response {
    let query = spec_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        let q = query?;
        let review = state
            .review
            .as_deref()
            .ok_or_else(|| unconfigured("review_unconfigured"))?;
        let specs = widget_bounded(
            &state,
            "portal_widget_spec_status",
            review.portal_spec_status(&w.proof, q.projects.as_deref()),
        )
        .await?;
        let title = match q.show {
            SpecShow::Approval => Word::TitleSpecApproval,
            SpecShow::Undecided => Word::TitleSpecUndecided,
            SpecShow::Lint => Word::TitleSpecLint,
        };
        envelope(
            "status",
            title.in_lang(q.lang),
            None,
            STALE_SPEC_S,
            spec_body(&specs, q.show, q.lang),
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
        let options = widget_bounded(
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
/// the same status and headers (contract §6.3) — except `Cache-Control`,
/// which is `no-store` on every failure (review S4: a refusal is never
/// kept by the portal or anything between). The standard body's
/// `error_code` and `user_message` become `code` and `message`; nothing
/// else is added.
pub(crate) async fn envelope_errors(req: Request, next: Next) -> Response {
    let widget = req.uri().path().starts_with(WIDGET_PREFIX);
    let res = next.run(req).await;
    if !widget || res.status().is_success() {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    parts
        .headers
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
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
