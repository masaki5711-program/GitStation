//! End-to-end tests of the account commands of M2 MR6 (`token list`,
//! `token revoke`, `user search`, `user list`, `user disable`, `user
//! sessions`, `user end-sessions`) against a scripted loopback server.
//! All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

fn ok(body: &str) -> Resp {
    Resp::json(200, body)
}

fn last(srv: &Mock) -> (String, String) {
    let r = srv.seen().pop().expect("a request");
    (r.method, r.target)
}

const TOKENS: &str = r#"{"items":[{"id":31,"public_id":"00000000000000a1","name":"ci","scopes":["read_api"],"created_at":"2026-01-01T00:00:00Z","expires_at":"2026-04-01T00:00:00Z","revoked_at":null,"last_used_at":null}]}"#;

#[test]
fn token_list_and_revoke_call_the_own_token_routes() {
    let srv = Mock::start(|r| {
        if r.method == "DELETE" {
            Resp::json(204, "")
        } else {
            ok(TOKENS)
        }
    });
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["token", "list", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(last(&srv), ("GET".into(), "/api/v1/user/tokens".into()));
    assert_eq!(o.json()["items"][0]["name"], "ci");
    let h = gstn(&sb, &srv, &["token", "list"]);
    assert!(
        h.stdout.contains("31") && h.stdout.contains("ci"),
        "{}",
        h.stdout
    );

    // Destructive: --yes first, before any request.
    let before = srv.seen().len();
    assert_eq!(gstn(&sb, &srv, &["token", "revoke", "31"]).code, 2);
    assert_eq!(srv.seen().len(), before);
    let o = gstn(&sb, &srv, &["token", "revoke", "31", "--yes", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        last(&srv),
        ("DELETE".into(), "/api/v1/user/tokens/31".into())
    );
    assert_eq!(o.json(), json!({"id": 31, "revoked": true}));
    // An id is a positive whole number (never text in the path).
    assert_eq!(
        gstn(&sb, &srv, &["token", "revoke", "../1", "--yes"]).code,
        2
    );
}

#[test]
fn user_search_and_list_send_their_query() {
    let srv = Mock::start(|r| {
        if r.target.starts_with("/api/v1/users") {
            ok(r#"{"items":[{"id":3,"username":"alice","display_name":"Alice"}]}"#)
        } else {
            ok(
                r#"{"items":[{"id":3,"kind":"human","username":"alice","display_name":"Alice","state":"disabled","is_admin":false,"created_at":"2026-01-01T00:00:00Z"}],"next_cursor":"c-2","has_more":true}"#,
            )
        }
    });
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["user", "search", "al ice", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        last(&srv),
        ("GET".into(), "/api/v1/users?search=al+ice".into())
    );
    assert_eq!(o.json()["items"][0]["username"], "alice");
    let o = gstn(
        &sb,
        &srv,
        &[
            "user", "list", "--state", "disabled", "--limit", "5", "--cursor", "c-1",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        last(&srv),
        (
            "GET".into(),
            "/api/v1/admin/users?state=disabled&limit=5&cursor=c-1".into()
        )
    );
    assert!(o.stdout.contains("more: --cursor c-2"), "{}", o.stdout);
    assert_eq!(
        gstn(&sb, &srv, &["user", "list", "--state", "gone"]).code,
        2
    );
}

#[test]
fn user_disable_and_sessions_are_the_administration_routes() {
    let srv = Mock::start(|r| match (r.method.as_str(), r.target.as_str()) {
        ("POST", _) => ok(r#"{"user_id":12,"outcome":"disabled"}"#),
        ("DELETE", _) => ok(r#"{"user_id":12,"revoked_sessions":2}"#),
        _ => ok(
            r#"{"items":[{"public_id":"00000000000000b1","created_at":"2026-01-01T00:00:00Z","last_seen_at":"2026-01-01T00:00:00Z","idle_expires_at":"2026-01-01T04:00:00Z","absolute_expires_at":"2026-01-01T12:00:00Z","oidc":true,"current":false}],"truncated":false}"#,
        ),
    });
    let sb = Sandbox::new();
    for args in [
        &["user", "disable", "12"][..],
        &["user", "end-sessions", "12"][..],
    ] {
        assert_eq!(gstn(&sb, &srv, args).code, 2, "{args:?} needs --yes");
    }
    assert!(srv.seen().is_empty(), "nothing sent without --yes");
    let o = gstn(&sb, &srv, &["user", "disable", "12", "--yes"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        last(&srv),
        ("POST".into(), "/api/v1/admin/users/12/disable".into())
    );
    assert!(o.stdout.contains("User 12: disabled"), "{}", o.stdout);
    let o = gstn(&sb, &srv, &["user", "sessions", "12", "--json"]);
    assert_eq!(
        last(&srv),
        ("GET".into(), "/api/v1/admin/users/12/sessions".into())
    );
    assert_eq!(o.json()["items"][0]["public_id"], "00000000000000b1");
    let o = gstn(
        &sb,
        &srv,
        &["user", "end-sessions", "12", "--yes", "--json"],
    );
    assert_eq!(
        last(&srv),
        ("DELETE".into(), "/api/v1/admin/users/12/sessions".into())
    );
    assert_eq!(o.json(), json!({"user_id": 12, "revoked_sessions": 2}));
}

#[test]
fn the_new_commands_are_described_with_their_scopes() {
    let srv = Mock::start(|_| ok("{}"));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["help", "--json"]);
    let v: Value = o.json();
    let scopes = |name: &str| {
        v["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing"))["scopes"]
            .clone()
    };
    assert_eq!(scopes("token list"), json!(["read_api"]));
    assert_eq!(scopes("token revoke"), json!(["api"]));
    assert_eq!(scopes("user search"), json!(["read_api"]));
    assert_eq!(scopes("user disable"), json!(["api", "admin"]));
    assert_eq!(scopes("user end-sessions"), json!(["api", "admin"]));
}
