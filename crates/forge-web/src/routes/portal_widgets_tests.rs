//! The widget routes' settings and bodies (ADR-0036), without a database.

use axum::http::StatusCode;
use forge_errors::{ErrorCode, ForgeError, Lang};
use tower::ServiceExt;

use super::*;

fn rule(e: &ForgeError) -> (String, String) {
    let get = |key: &str| {
        e.context()
            .iter()
            .find(|(k, _)| *k == key)
            .map_or_else(String::new, |(_, v)| v.clone())
    };
    (get("param"), get("rule"))
}

#[test]
fn the_settings_default_as_declared() {
    let q = reviews_query(None).expect("defaults");
    assert_eq!(q.kind, WidgetList::Review);
    assert_eq!(q.limit, DEFAULT_LIMIT);
    assert_eq!(q.project, None);
    assert_eq!(q.lang, Lang::En);
    assert_eq!(
        q.order,
        WidgetOrder::Oldest,
        "what has waited longest first"
    );
    let p = pipelines_query(Some("")).expect("defaults");
    assert!(!p.only_problems);
    assert_eq!(p.show, Show::Pipeline);
    assert_eq!(p.projects, None);
    assert_eq!(
        stopped_query(None).expect("defaults").0,
        WidgetList::StoppedMine
    );
}

#[test]
fn the_settings_are_read_as_the_contract_sends_them() {
    let q = reviews_query(Some(
        "kind=spec&project=7&limit=10&locale=id&tz=Asia%2FJakarta&x=1",
    ))
    .expect("valid");
    assert_eq!(q.kind, WidgetList::Spec);
    assert_eq!(q.project, Some(ProjectId::new(7)));
    assert_eq!(q.limit, 10);
    assert_eq!(q.lang, Lang::Id);
    // `multi` repeats the key; duplicates count once.
    let p = pipelines_query(Some("project=3&project=4&project=3&show=deploy")).expect("valid");
    assert_eq!(p.projects, Some(vec![ProjectId::new(3), ProjectId::new(4)]));
    assert_eq!(p.show, Show::Deploy);
    let (scope, lang) = stopped_query(Some("scope=reviews&locale=ja")).expect("valid");
    assert_eq!((scope, lang), (WidgetList::StoppedInvolved, Lang::Ja));
}

#[test]
fn invalid_settings_name_their_param_and_rule() {
    let cases: &[(&str, (&str, &str))] = &[
        ("kind=all", ("kind", "unknown_value")),
        ("limit=2", ("limit", "out_of_range")),
        ("limit=11", ("limit", "out_of_range")),
        ("project=-1", ("project", "not_positive_decimal")),
        ("limit=3&limit=4", ("limit", "repeated")),
        ("locale=de", ("locale", "unknown_value")),
        ("tz=a b", ("tz", "invalid")),
    ];
    for (raw, want) in cases {
        let e = reviews_query(Some(raw)).expect_err(raw);
        assert_eq!(e.code(), ErrorCode::PortalWidgetSettingInvalid, "{raw}");
        assert_eq!(rule(&e), (want.0.to_owned(), want.1.to_owned()), "{raw}");
    }
    let many: Vec<String> = (1..=25).map(|n| format!("project={n}")).collect();
    let e = pipelines_query(Some(&many.join("&"))).expect_err("too many");
    assert_eq!(rule(&e), ("project".to_owned(), "too_many".to_owned()));
    assert_eq!(
        ErrorCode::PortalWidgetSettingInvalid.http_status(),
        422,
        "the contract's invalid value"
    );
}

/// A setting's check, its value dropped.
type Checked = Result<(), ForgeError>;

#[test]
fn the_p1_settings_are_read_and_checked() {
    for (raw, kind) in [
        ("kind=threads", WidgetList::Threads),
        ("kind=conflicts", WidgetList::Conflicts),
        ("kind=queue", WidgetList::Queue),
    ] {
        assert_eq!(reviews_query(Some(raw)).expect(raw).kind, kind);
    }
    assert_eq!(
        reviews_query(Some("sort=newest")).expect("newest").order,
        WidgetOrder::Newest
    );
    assert_eq!(
        reviews_query(Some("sort=oldest")).expect("oldest").order,
        WidgetOrder::Oldest
    );
    assert!(
        pipelines_query(Some("only_problems=true"))
            .expect("on")
            .only_problems
    );
    assert!(
        !pipelines_query(Some("only_problems=false"))
            .expect("off")
            .only_problems
    );
    let a = attention_query(None).expect("defaults");
    assert_eq!((a.scope, a.projects), (AttentionScope::Reviews, None));
    for (raw, scope) in [
        ("scope=stopped", AttentionScope::Stopped),
        ("scope=reviews", AttentionScope::Reviews),
        ("scope=spec", AttentionScope::Spec),
        ("scope=failed", AttentionScope::Failed),
    ] {
        assert_eq!(attention_query(Some(raw)).expect(raw).scope, scope);
    }
    let a = attention_query(Some("scope=stopped&project=4&project=2&project=4")).expect("multi");
    assert_eq!(a.projects, Some(vec![ProjectId::new(4), ProjectId::new(2)]));
    let m = menu_query(None).expect("defaults");
    assert_eq!((m.purpose, m.project), (MenuPurpose::Developer, None));
    let m = menu_query(Some("purpose=manager&project=9&locale=ja")).expect("menu");
    assert_eq!(
        (m.purpose, m.project, m.lang),
        (MenuPurpose::Manager, Some(ProjectId::new(9)), Lang::Ja)
    );
    let invalid: Vec<(Checked, (&str, &str))> = vec![
        (
            reviews_query(Some("sort=random")).map(drop),
            ("sort", "unknown_value"),
        ),
        (
            reviews_query(Some("kind=deploy")).map(drop),
            ("kind", "unknown_value"),
        ),
        (
            pipelines_query(Some("only_problems=yes")).map(drop),
            ("only_problems", "not_boolean"),
        ),
        (
            attention_query(Some("scope=deploy")).map(drop),
            ("scope", "not_available"),
        ),
        (
            attention_query(Some("scope=all")).map(drop),
            ("scope", "unknown_value"),
        ),
        (
            attention_query(Some("project=x")).map(drop),
            ("project", "not_positive_decimal"),
        ),
        (
            menu_query(Some("purpose=admin")).map(drop),
            ("purpose", "unknown_value"),
        ),
        (
            menu_query(Some("project=1&project=2")).map(drop),
            ("project", "repeated"),
        ),
    ];
    for (result, want) in invalid {
        let e = result.expect_err(want.0);
        assert_eq!(e.code(), ErrorCode::PortalWidgetSettingInvalid);
        assert_eq!(rule(&e), (want.0.to_owned(), want.1.to_owned()));
    }
    let many: Vec<String> = (1..=25).map(|n| format!("project={n}")).collect();
    let e = attention_query(Some(&many.join("&"))).expect_err("too many");
    assert_eq!(rule(&e), ("project".to_owned(), "too_many".to_owned()));
}

/// The SPA's sources (`web/src/…`), read by the menu test.
fn web_source(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/src")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The words of a TypeScript `const NAME = [...]` list in `source`.
fn ts_list(source: &str, name: &str) -> Vec<String> {
    let start = source
        .find(&format!("const {name}"))
        .unwrap_or_else(|| panic!("{name} not found"));
    let rest = &source[start..];
    let open = rest.find("= [").expect("= [") + 2;
    let close = rest[open..].find(']').expect("]") + open;
    rest[open + 1..close]
        .split(',')
        .map(|w| w.trim().trim_matches('\'').to_owned())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Every menu link opens a screen the SPA has (widget design §4): an
/// application path is a `<Route path="-/…">` of `App.tsx`, a project path
/// a screen word of `RepoViews.tsx` that needs no further segment, and
/// every query key is one the screen reads (with a value it knows).
#[test]
fn every_menu_href_is_a_screen_of_the_spa() {
    let app = web_source("App.tsx");
    let repo = web_source("pages/repo/RepoViews.tsx");
    let words = ts_list(&repo, "SCREEN_WORDS");
    let scopes = ts_list(&web_source("api/userMergeRequests.ts"), "MR_SCOPES");
    let states = ts_list(
        &web_source("pages/repo/mr/MergeRequestsScreen.tsx"),
        "STATES",
    );
    // The screens a query is read on, and the values each key may take
    // (`None`: any value the screen parses).
    let readers: &[(&str, &str, &str, Option<&[String]>)] = &[
        (
            "-/merge-requests",
            "pages/MyMergeRequests.tsx",
            "scope",
            Some(scopes.as_slice()),
        ),
        ("-/projects", "pages/Projects.tsx", "tab", None),
        ("-/notifications", "pages/Inbox.tsx", "unread", None),
        (
            "pulls",
            "pages/repo/mr/MergeRequestsScreen.tsx",
            "state",
            Some(states.as_slice()),
        ),
    ];
    let mut checked = 0;
    for purpose in MenuPurpose::ALL {
        for project in [Some("acme/web"), None] {
            for link in menu_links(purpose, project) {
                let (path, query) = link.href.split_once('?').unwrap_or((&link.href, ""));
                let screen = if let Some(app_path) = path.strip_prefix("/-/") {
                    let route = format!("-/{app_path}");
                    assert!(
                        app.contains(&format!("path=\"{route}\"")),
                        "{}: no <Route path=\"{route}\"> in App.tsx",
                        link.href
                    );
                    route
                } else if path == "/" {
                    "/".to_owned()
                } else {
                    let (_, view) = path
                        .split_once("/-/")
                        .unwrap_or_else(|| panic!("{}: not a project screen", link.href));
                    let mut segments = view.split('/');
                    let word = segments.next().unwrap_or_default().to_owned();
                    assert!(words.contains(&word), "{}: {word} is no screen", link.href);
                    assert_eq!(segments.next(), None, "{}: one segment", link.href);
                    assert!(
                        !["edit", "new", "pull", "commit", "blob"].contains(&word.as_str()),
                        "{}: {word} needs more segments",
                        link.href
                    );
                    word
                };
                for pair in query.split('&').filter(|p| !p.is_empty()) {
                    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                    let Some((_, file, _, values)) = readers
                        .iter()
                        .find(|(s, _, k, _)| *s == screen && *k == key)
                    else {
                        panic!("{}: the screen {screen} reads no `{key}`", link.href);
                    };
                    assert!(
                        web_source(file).contains(&format!("get('{key}')")),
                        "{}: {file} reads no `{key}`",
                        link.href
                    );
                    if let Some(values) = values {
                        assert!(
                            values.iter().any(|v| v == value),
                            "{}: `{value}` is not a value of `{key}`",
                            link.href
                        );
                    } else {
                        assert!(
                            web_source(file).contains(&format!("'{value}'")),
                            "{}: {file} knows no `{value}`",
                            link.href
                        );
                    }
                }
                checked += 1;
            }
        }
    }
    assert!(checked >= 30, "{checked}");
    // The merge queue screen reads `target` (the `queue` list's links).
    assert!(web_source("pages/repo/mr/MergeQueueScreen.tsx").contains("get('target')"));
    assert!(words.iter().any(|w| w == "merge-queue"));
}

/// ADR-0021 amendment 2026-11-24: the administrator link comes last, only
/// when asked for, and its screen is a route of the SPA that sends the
/// person on to their administrator settings.
#[test]
fn the_admin_link_is_last_and_a_screen_of_the_spa() {
    let plain = menu_body(MenuPurpose::Developer, None, false, Lang::Ja);
    let admin = menu_body(MenuPurpose::Developer, None, true, Lang::Ja);
    let plain = plain["items"].as_array().expect("items");
    let admin = admin["items"].as_array().expect("items");
    assert_eq!(admin.len(), plain.len() + 1);
    assert!(plain.iter().all(|i| i["href"] != "/-/admin"));
    let last = admin.last().expect("a link");
    assert_eq!(last["href"], "/-/admin");
    assert_eq!(last["label"], "管理者の設定");
    assert!(web_source("App.tsx").contains("path=\"-/admin\""));
    assert!(web_source("pages/AdminRedirect.tsx").contains("tab=admin"));
}

#[test]
fn the_p1_bodies_follow_the_contract() {
    let links = menu_body(MenuPurpose::Spec, Some("acme/spec"), false, Lang::Ja);
    let items = links["items"].as_array().expect("items");
    assert!(!items.is_empty() && items.len() <= 20);
    assert!(items.iter().all(|i| i["label"]
        .as_str()
        .is_some_and(|l| !l.is_empty() && l.len() <= 120)));
    assert_eq!(items[0]["href"], "/acme/spec/-/documents");
    let inbox = inbox_body(100, true, Lang::En);
    assert_eq!(inbox["value"], 100);
    assert_eq!(inbox["display"], "100+ unread");
    let failed = kpi_body_of(
        UserListCount {
            count: 2,
            capped: false,
            truncated: true,
        },
        Word::UnitProjects,
        Tone::Stop,
        Lang::En,
    );
    assert_eq!(failed["display"], "2+ projects");
    assert_eq!(failed["delta"]["tone"], "stop");
    let reviews = kpi_body_of(
        UserListCount {
            count: 3,
            capped: false,
            truncated: false,
        },
        Word::UnitChanges,
        Tone::Warn,
        Lang::Id,
    );
    assert_eq!(reviews["delta"]["tone"], "warn");
}

#[test]
fn the_kpi_compares_with_the_standard_zero() {
    let zero = kpi_body(
        UserListCount {
            count: 0,
            capped: false,
            truncated: false,
        },
        Lang::Ja,
    );
    assert_eq!(zero["value"], 0);
    assert_eq!(zero["reference"]["value"], 0);
    assert_eq!(zero["delta"]["tone"], "ok");
    let capped = kpi_body(
        UserListCount {
            count: 100,
            capped: true,
            truncated: false,
        },
        Lang::En,
    );
    assert_eq!(capped["display"], "100+ changes");
    assert_eq!(capped["delta"]["tone"], "stop");
    assert!(capped["delta"]["display"]
        .as_str()
        .is_some_and(|d| d.contains("100+")));
}

#[tokio::test]
async fn a_widget_failure_answers_in_the_contracts_envelope() {
    async fn failing() -> Response {
        let body = json!({
            "error_code": "FG-AUTH-0003",
            "user_message": "no",
            "request_id": "r-1",
        });
        (StatusCode::FORBIDDEN, Json(body)).into_response()
    }
    // The middleware around a bare service: routes are added only through
    // the registry (ADR-0010), so no router is built here.
    let inner = tower::service_fn(|_request: axum::http::Request<Body>| async {
        Ok::<_, std::convert::Infallible>(failing().await)
    });
    let app = tower::Layer::layer(&axum::middleware::from_fn(envelope_errors), inner);
    let call = |uri: &'static str| {
        let app = app.clone();
        async move {
            let res = app
                .oneshot(
                    axum::http::Request::get(uri)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("infallible");
            let status = res.status();
            let cache = res
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let bytes = axum::body::to_bytes(res.into_body(), 1 << 16)
                .await
                .expect("body");
            (
                status,
                serde_json::from_slice::<Value>(&bytes).expect("json"),
                cache,
            )
        }
    };
    let (status, wrapped, cache) = call(MY_REVIEWS).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        wrapped,
        json!({"success": false, "error": {"code": "FG-AUTH-0003", "message": "no", "request_id": "r-1"}})
    );
    // Review S4: a widget's failure is never kept.
    assert_eq!(cache.as_deref(), Some("no-store"));
    // Any other route keeps the standard body (and its own headers).
    let (_, plain, cache) = call("/api/v1/other").await;
    assert_eq!(plain["error_code"], "FG-AUTH-0003");
    assert_eq!(cache, None);
}

fn context_of(e: &ForgeError, key: &str) -> Option<String> {
    e.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.clone())
}

/// Review S3: the gate bounds the widget requests running at once and
/// their rate, as one caller (the portal).
#[test]
fn the_widget_gate_bounds_the_running_requests_and_the_rate() {
    let now = Instant::now();
    // Two at once: the third waits for one to end.
    let gate = WidgetGate::new(2, 100, Duration::from_secs(60));
    let first = gate.enter(now).expect("first");
    let _second = gate.enter(now).expect("second");
    let full = gate.enter(now).expect_err("two are running");
    assert_eq!(full.code(), ErrorCode::RateLimited);
    assert_eq!(
        context_of(&full, "route").as_deref(),
        Some("portal_widgets")
    );
    assert_eq!(context_of(&full, "reason").as_deref(), Some("in_flight"));
    assert_eq!(
        context_of(&full, crate::error::RETRY_AFTER_CONTEXT).as_deref(),
        Some("1")
    );
    drop(first);
    drop(gate.enter(now).expect("a place again"));
    // Three a minute: the fourth is refused until the window ends (the
    // one refused for the running ones above did not count).
    let gate = WidgetGate::new(8, 3, Duration::from_secs(60));
    for _ in 0..3 {
        drop(gate.enter(now).expect("within the rate"));
    }
    let over = gate
        .enter(now + Duration::from_secs(20))
        .expect_err("over the rate");
    assert_eq!(over.code(), ErrorCode::RateLimited);
    assert_eq!(
        context_of(&over, "route").as_deref(),
        Some("portal_widgets")
    );
    assert_eq!(context_of(&over, "reason"), None);
    assert_eq!(
        context_of(&over, crate::error::RETRY_AFTER_CONTEXT).as_deref(),
        Some("40")
    );
    // The refused request left no place taken, and the next window is open.
    drop(
        gate.enter(now + Duration::from_secs(61))
            .expect("a new window"),
    );
}

// ------------------------------------------------------------ widget design P2a

#[test]
fn the_p2a_settings_are_read_and_checked() {
    let q = queue_query(None).expect("defaults");
    assert_eq!(q.projects, None);
    assert_eq!(q.lang, Lang::En);
    let q = queue_query(Some("project=3&project=4&project=3&locale=ja&size=M")).expect("q");
    assert_eq!(q.projects, Some(vec![ProjectId::new(3), ProjectId::new(4)]));
    assert_eq!(q.lang, Lang::Ja);
    let s = spec_query(None).expect("defaults");
    assert_eq!(s.show, SpecShow::Approval);
    assert_eq!(s.projects, None);
    for (raw, show) in [
        ("show=approval", SpecShow::Approval),
        ("show=undecided", SpecShow::Undecided),
        ("show=lint", SpecShow::Lint),
    ] {
        assert_eq!(spec_query(Some(raw)).expect(raw).show, show);
    }
    let many = (1..=25)
        .map(|n| format!("project={n}"))
        .collect::<Vec<_>>()
        .join("&");
    for (raw, param, why) in [
        ("show=everything", "show", "unknown_value"),
        ("show=lint&show=approval", "show", "repeated"),
        ("project=0", "project", "not_positive_decimal"),
        (many.as_str(), "project", "too_many"),
        ("locale=fr", "locale", "unknown_value"),
    ] {
        let e = spec_query(Some(raw)).expect_err(raw);
        assert_eq!(e.code(), ErrorCode::PortalWidgetSettingInvalid, "{raw}");
        assert_eq!(rule(&e), (param.to_owned(), why.to_owned()), "{raw}");
    }
    for raw in ["project=x", many.as_str(), "tz=../etc"] {
        let e = queue_query(Some(raw)).expect_err(raw);
        assert_eq!(e.code(), ErrorCode::PortalWidgetSettingInvalid, "{raw}");
    }
}

fn queue(queued: i64, testing: i64, ejected: i64, paused: bool) -> QueueStatus {
    QueueStatus {
        project: ProjectId::new(7),
        target_branch: "refs/heads/main".to_owned(),
        paused,
        queued,
        testing,
        ejected,
    }
}

#[test]
fn a_queues_lamp_says_what_waits_on_a_person_first() {
    let lamp = |q: QueueStatus| queue_lamp(&q, Lang::En);
    assert_eq!(
        lamp(queue(0, 0, 0, false)),
        (Lamp::Ok, "Nothing waiting".into())
    );
    assert_eq!(lamp(queue(0, 2, 0, false)), (Lamp::Ok, "Running".into()));
    assert_eq!(
        lamp(queue(3, 1, 0, false)),
        (Lamp::Info, "3 waiting".into())
    );
    assert_eq!(lamp(queue(0, 0, 0, true)), (Lamp::Warn, "Paused".into()));
    assert_eq!(
        lamp(queue(2, 0, 0, true)),
        (Lamp::Warn, "Paused (2 waiting)".into())
    );
    // A removed change wins over everything: someone must act on it.
    assert_eq!(
        lamp(queue(2, 1, 1, true)),
        (Lamp::Stop, "1 removed from the queue".into())
    );
    // The reads stop at their cap: said, never silently low.
    assert_eq!(
        lamp(queue(MAX_STATUS_COUNT, 0, 0, false)),
        (Lamp::Info, "100+ waiting".into())
    );
    assert_eq!(queue_lamp(&queue(3, 0, 0, false), Lang::Ja).1, "3 件待ち");
}

#[test]
fn the_queue_body_links_each_queue_of_its_branch() {
    let main = PortalQueue {
        path: "acme/web".to_owned(),
        status: queue(1, 0, 0, false),
    };
    let release = PortalQueue {
        path: "acme/web".to_owned(),
        status: QueueStatus {
            target_branch: "refs/heads/rel/1.0".to_owned(),
            ..queue(0, 0, 0, false)
        },
    };
    let body = queue_body(&[main, release], Lang::En);
    let items = body["items"].as_array().expect("items");
    assert_eq!(items[0]["key"], "7:main");
    assert_eq!(items[0]["label"], "acme/web");
    assert_eq!(items[0]["state"], "info");
    assert_eq!(items[0]["href"], "/acme/web/-/merge-queue?target=main");
    assert_eq!(items[1]["key"], "7:rel/1.0");
    assert_eq!(items[1]["label"], "acme/web (rel/1.0)");
    assert_eq!(items[1]["href"], "/acme/web/-/merge-queue?target=rel/1.0");
    // Empty: the portal shows its own empty state.
    assert_eq!(queue_body(&[], Lang::En), json!({ "items": [] }));
}

fn spec(awaiting: i64, undecided: i64, partial: bool, failing: i64, unchecked: i64) -> SpecStatus {
    SpecStatus {
        project: ProjectId::new(9),
        awaiting,
        undecided,
        undecided_partial: partial,
        failing,
        unchecked,
    }
}

#[test]
fn a_specs_lamp_never_says_nothing_found_before_the_checks_answered() {
    use SpecShow::{Approval, Lint, Undecided};
    let lamp = |s: SpecStatus, show| spec_lamp(&s, show, Lang::En);
    assert_eq!(
        lamp(spec(0, 0, false, 0, 0), Approval),
        (Lamp::Ok, "Nothing awaiting approval".into())
    );
    assert_eq!(
        lamp(spec(2, 0, false, 0, 2), Approval),
        (Lamp::Warn, "2 awaiting approval".into())
    );
    assert_eq!(
        lamp(spec(1, 3, false, 1, 0), Undecided),
        (Lamp::Warn, "3 undecided".into())
    );
    assert_eq!(
        lamp(spec(1, 50, true, 1, 0), Undecided),
        (Lamp::Warn, "50+ undecided".into())
    );
    assert_eq!(
        lamp(spec(1, 0, false, 0, 1), Undecided),
        (Lamp::Unknown, "Being checked".into())
    );
    assert_eq!(
        lamp(spec(1, 0, false, 0, 0), Undecided),
        (Lamp::Ok, "Nothing undecided".into())
    );
    assert_eq!(
        lamp(spec(3, 0, false, 2, 1), Lint),
        (Lamp::Stop, "2 with check findings".into())
    );
    assert_eq!(
        lamp(spec(3, 0, false, 0, 1), Lint),
        (Lamp::Unknown, "Being checked".into())
    );
    assert_eq!(
        lamp(spec(3, 0, false, 0, 0), Lint),
        (Lamp::Ok, "No findings".into())
    );
    assert_eq!(
        lamp(spec(MAX_STATUS_COUNT, 0, false, 0, 0), Approval).1,
        "100+ awaiting approval"
    );
    assert_eq!(
        spec_lamp(&spec(2, 0, false, 0, 0), Approval, Lang::Ja).1,
        "承認待ち 2 件"
    );
}

#[test]
fn the_spec_body_opens_the_documents_or_the_open_changes() {
    let p = PortalSpec {
        path: "acme/spec".to_owned(),
        status: spec(1, 1, false, 0, 0),
    };
    let undecided = spec_body(std::slice::from_ref(&p), SpecShow::Undecided, Lang::En);
    assert_eq!(undecided["items"][0]["key"], "9");
    assert_eq!(undecided["items"][0]["label"], "acme/spec");
    assert_eq!(undecided["items"][0]["href"], "/acme/spec/-/documents");
    for show in [SpecShow::Approval, SpecShow::Lint] {
        let body = spec_body(std::slice::from_ref(&p), show, Lang::En);
        assert_eq!(body["items"][0]["href"], "/acme/spec/-/pulls?state=open");
    }
}

/// The reads of one widget stop at 2.5 s whatever `server.lookup_timeout`
/// says, as `FG-WEB-0002` naming the widget (the portal's limit is 3 s).
#[tokio::test(start_paused = true)]
async fn a_slow_widget_read_stops_inside_the_portals_limit() {
    assert_eq!(widget_limit(Duration::from_secs(10)), WIDGET_READ_BUDGET);
    assert_eq!(
        widget_limit(Duration::from_millis(800)),
        Duration::from_millis(800)
    );
    let started = tokio::time::Instant::now();
    let e = bounded_by(
        widget_limit(Duration::from_secs(10)),
        "portal_widget_merge_queue",
        std::future::pending::<Result<(), ForgeError>>(),
    )
    .await
    .expect_err("timed out");
    assert_eq!(e.code(), ErrorCode::RequestTimeout);
    assert_eq!(
        context_of(&e, "operation").as_deref(),
        Some("portal_widget_merge_queue")
    );
    assert_eq!(context_of(&e, "timeout_ms").as_deref(), Some("2500"));
    assert_eq!(started.elapsed(), WIDGET_READ_BUDGET);
    // A read inside the limit answers as it is.
    let ok = bounded_by(WIDGET_READ_BUDGET, "x", async { Ok::<_, ForgeError>(5) }).await;
    assert_eq!(ok.expect("in time"), 5);
}
