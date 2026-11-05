//! End-to-end tests of `gstn notifications get|set` (M3 N1) against a
//! scripted loopback server. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

const KINDS: [&str; 3] = ["mr_approved", "mr_merged", "mr_commented"];

fn prefs(email: bool, language: Value, muted: &[&str]) -> String {
    json!({"email_enabled": email, "language": language, "muted_kinds": muted, "kinds": KINDS})
        .to_string()
}

/// Answers GET with `stored` and echoes a PUT body back as the new settings.
fn server(stored: String) -> Mock {
    Mock::start(move |r| {
        if r.method == "PUT" {
            let mut body: Value = serde_json::from_str(&r.body).expect("json body");
            body["kinds"] = json!(KINDS);
            Resp::json(200, &body.to_string())
        } else {
            Resp::json(200, &stored)
        }
    })
}

#[test]
fn get_reads_the_own_settings_route() {
    let srv = server(prefs(true, Value::Null, &["mr_commented"]));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["notifications", "get", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let seen = srv.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].target.as_str()),
        ("GET", "/api/v1/user/notification-preferences")
    );
    assert_eq!(o.json()["muted_kinds"], json!(["mr_commented"]));
    let h = gstn(&sb, &srv, &["notifications", "get"]);
    assert_eq!(h.code, 0, "{}", h.stderr);
    assert!(
        h.stdout.contains("mr_commented") && h.stdout.to_lowercase().contains("on"),
        "{}",
        h.stdout
    );
}

#[test]
fn set_changes_only_what_it_is_told_and_keeps_the_rest() {
    let srv = server(prefs(true, json!("ja"), &["mr_commented"]));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "notifications",
            "set",
            "--email",
            "off",
            "--mute",
            "mr_merged",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let seen = srv.seen();
    // It reads the current settings, then replaces them as a whole.
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0].method, "GET");
    assert_eq!(
        (seen[1].method.as_str(), seen[1].target.as_str()),
        ("PUT", "/api/v1/user/notification-preferences")
    );
    let sent: Value = serde_json::from_str(&seen[1].body).expect("body");
    assert_eq!(
        sent,
        json!({"email_enabled": false, "language": "ja", "muted_kinds": ["mr_commented", "mr_merged"]})
    );
    assert_eq!(o.json()["email_enabled"], false);
}

#[test]
fn set_can_unmute_change_the_language_and_go_back_to_the_default() {
    let srv = server(prefs(true, json!("ja"), &["mr_commented", "mr_merged"]));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "notifications",
            "set",
            "--unmute",
            "mr_commented",
            "--language",
            "id",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let sent: Value = serde_json::from_str(&srv.seen()[1].body).expect("body");
    assert_eq!(
        sent,
        json!({"email_enabled": true, "language": "id", "muted_kinds": ["mr_merged"]})
    );
    let o = gstn(
        &sb,
        &srv,
        &["notifications", "set", "--language", "default", "--json"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let sent: Value = serde_json::from_str(&srv.seen().pop().expect("request").body).expect("body");
    assert_eq!(sent["language"], Value::Null);
}

#[test]
fn set_with_nothing_to_change_is_a_usage_error_before_any_request() {
    let srv = server(prefs(true, Value::Null, &[]));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["notifications", "set"]);
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(srv.seen().is_empty());
    // A flag value gstn knows is wrong is refused locally too.
    for args in [
        vec!["notifications", "set", "--email", "maybe"],
        vec!["notifications", "set", "--language", "fr"],
        vec!["notifications", "set", "--mute", ""],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 2, "{args:?}: {}", o.stderr);
    }
    assert!(srv.seen().is_empty());
}

#[test]
fn a_server_refusal_is_shown_with_its_request_id() {
    let srv = Mock::start(|_| {
        Resp::json(
            400,
            r#"{"error_code":"FG-WEB-0006","request_id":"req-abc","user_message":"The request is not valid."}"#,
        )
    });
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["notifications", "get"]);
    assert_ne!(o.code, 0);
    assert!(
        o.stderr.contains("req-abc") && o.stderr.contains("FG-WEB-0006"),
        "{}",
        o.stderr
    );
}
