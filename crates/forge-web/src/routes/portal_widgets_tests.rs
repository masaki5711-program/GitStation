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
    let p = pipelines_query(Some("")).expect("defaults");
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
