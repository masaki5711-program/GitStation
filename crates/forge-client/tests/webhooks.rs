//! End-to-end tests of `gstn webhook …` (M3 I2) against a scripted loopback
//! server: the routes and bodies each command sends, the secret shown once,
//! the confirmation of a delete, and nothing sent for bad input. All data
//! is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

const HOOK: &str = r#"{"id":3,"project_id":7,"owner_id":5,"origin":"https://kanflow.internal:443","event_types":["merge_request.*"],"enabled":true,"disabled_reason":null,"secret_version":1,"previous_secret_valid_until":null,"consecutive_failures":0,"failing_since":null,"last_delivery_at":null,"created_at":"2026-11-05T00:00:00Z","updated_at":"2026-11-05T00:00:00Z"}"#;

#[test]
fn webhook_create_posts_the_url_and_types_and_shows_the_secret_once() {
    let created = format!(r#"{{"webhook":{HOOK},"secret":"whsec_synthetic"}}"#);
    let srv = Mock::start(move |_| Resp::json(201, &created));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "webhook",
            "create",
            "--project",
            "7",
            "--url",
            "https://kanflow.internal/hook",
            "--event",
            "merge_request.*",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("whsec_synthetic"), "{}", o.stdout);
    assert!(o.stdout.contains("shown once"), "{}", o.stdout);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("POST", "/api/v1/projects/7/webhooks")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"url": "https://kanflow.internal/hook", "event_types": ["merge_request.*"]})
    );
}

#[test]
fn webhook_commands_call_their_routes() {
    let list = format!(r#"{{"items":[{HOOK}]}}"#);
    let updated = format!(r#"{{"outcome":"updated","fields":["enabled"],"webhook":{HOOK}}}"#);
    let cases: Vec<(Vec<&str>, &str, &str, String)> = vec![
        (vec!["webhook", "list", "--project", "7"], "GET", "/api/v1/projects/7/webhooks", list),
        (
            vec!["webhook", "event-types", "--project", "7"],
            "GET",
            "/api/v1/projects/7/webhooks/event-types",
            r#"{"items":[{"name":"merge_request.opened","class":"integration"}],"classes":[{"class":"integration","subscribable":true}]}"#.to_owned(),
        ),
        (vec!["webhook", "view", "3", "--project", "7"], "GET", "/api/v1/projects/7/webhooks/3", HOOK.to_owned()),
        (vec!["webhook", "update", "3", "--project", "7", "--enable"], "PATCH", "/api/v1/projects/7/webhooks/3", updated),
        (
            vec!["webhook", "rotate-secret", "3", "--project", "7"],
            "POST",
            "/api/v1/projects/7/webhooks/3/secret",
            r#"{"secret":"whsec_new","secret_version":2,"previous_secret_valid_until":"2026-11-06T00:00:00Z"}"#.to_owned(),
        ),
        (
            vec!["webhook", "deliveries", "3", "--project", "7", "--limit", "5", "--before", "40"],
            "GET",
            "/api/v1/projects/7/webhooks/3/deliveries?before=40&limit=5",
            r#"{"items":[],"next_before":null}"#.to_owned(),
        ),
        (
            vec!["webhook", "redeliver", "3", "12", "--project", "7"],
            "POST",
            "/api/v1/projects/7/webhooks/3/deliveries/12/redeliver",
            r#"{"outcome":"queued"}"#.to_owned(),
        ),
    ];
    for (args, method, target, answer) in cases {
        let srv = Mock::start(move |_| Resp::json(200, &answer));
        let sb = Sandbox::new();
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 0, "{args:?}: {}", o.stderr);
        let r = srv.seen().pop().expect("a request");
        assert_eq!(
            (r.method.as_str(), r.target.as_str()),
            (method, target),
            "{args:?}"
        );
    }
}

#[test]
fn webhook_update_sends_only_what_it_names() {
    let updated =
        format!(r#"{{"outcome":"updated","fields":["url","event_types"],"webhook":{HOOK}}}"#);
    let srv = Mock::start(move |_| Resp::json(200, &updated));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "webhook",
            "update",
            "3",
            "--project",
            "7",
            "--url",
            "https://kanflow.internal/v2",
            "--event",
            "*",
            "--disable",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"url": "https://kanflow.internal/v2", "event_types": ["*"], "enabled": false})
    );
}

#[test]
fn webhook_refuses_bad_input_before_any_request() {
    let srv = Mock::start(|_| Resp::json(200, "{}"));
    let sb = Sandbox::new();
    for args in [
        vec!["webhook", "delete", "3", "--project", "7"],
        vec!["webhook", "update", "3", "--project", "7"],
        vec![
            "webhook",
            "update",
            "3",
            "--project",
            "7",
            "--enable",
            "--disable",
        ],
        vec!["webhook", "view", "0", "--project", "7"],
        vec![
            "webhook",
            "deliveries",
            "3",
            "--project",
            "7",
            "--limit",
            "101",
        ],
        vec!["webhook", "create", "--project", "7"],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_ne!(o.code, 0, "{args:?}");
    }
    assert!(srv.seen().is_empty(), "nothing was sent");
    // With --yes the delete is sent.
    let srv = Mock::start(|_| Resp::json(204, ""));
    let o = gstn(
        &sb,
        &srv,
        &["webhook", "delete", "3", "--project", "7", "--yes"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("DELETE", "/api/v1/projects/7/webhooks/3")
    );
}
