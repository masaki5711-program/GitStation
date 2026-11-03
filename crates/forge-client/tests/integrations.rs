//! End-to-end tests of the integration commands of M3 I1a (`admin
//! integration list|view|bindings`, `integration-binding list|set|remove`)
//! against a scripted loopback server. All data is synthetic.

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

fn last(srv: &Mock) -> (String, String, String) {
    let r = srv.seen().pop().expect("a request");
    (r.method, r.target, r.body)
}

const INTEGRATION: &str = r#"{"id":3,"kind":"kanflow","service_account":{"id":42,"username":"kanflow-sync"},"enabled":true,"event_types":["*"],"bindings":1,"created_by":1,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#;
const BINDINGS: &str = r#"{"items":[{"integration_id":3,"kind":"kanflow","project_id":7,"external_project_id":"kanflow:42","granted_membership":true,"created_by":5,"created_at":"2026-01-01T00:00:00Z"}]}"#;

#[test]
fn admin_integration_commands_read_the_administration_routes() {
    let srv = Mock::start(|r| match r.target.as_str() {
        "/api/v1/admin/integrations" => {
            ok(&format!(r#"{{"items":[{INTEGRATION}],"truncated":false}}"#))
        }
        "/api/v1/admin/integrations/3" => ok(INTEGRATION),
        _ => ok(BINDINGS),
    });
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["admin", "integration", "list", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let (m, t, _) = last(&srv);
    assert_eq!(
        (m.as_str(), t.as_str()),
        ("GET", "/api/v1/admin/integrations")
    );
    assert_eq!(o.json()["items"][0]["kind"], "kanflow");
    let h = gstn(&sb, &srv, &["admin", "integration", "view", "3"]);
    assert_eq!(h.code, 0, "{}", h.stderr);
    assert!(
        h.stdout.contains("kanflow-sync (42)") && h.stdout.contains("event_types: *"),
        "{}",
        h.stdout
    );
    let o = gstn(
        &sb,
        &srv,
        &["admin", "integration", "bindings", "3", "--json"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let (_, t, _) = last(&srv);
    assert_eq!(t, "/api/v1/admin/integrations/3/bindings");
    assert_eq!(o.json()["items"][0]["external_project_id"], "kanflow:42");
    // An id is a positive whole number (never text in the path).
    assert_eq!(
        gstn(&sb, &srv, &["admin", "integration", "view", "../3"]).code,
        2
    );
}

#[test]
fn integration_binding_commands_call_the_project_routes() {
    let srv = Mock::start(|r| match r.method.as_str() {
        "DELETE" => Resp::json(204, ""),
        "PUT" => ok(&format!(
            r#"{{"outcome":"bound","binding":{}}}"#,
            &BINDINGS[10..BINDINGS.len() - 2]
        )),
        _ => ok(BINDINGS),
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &["integration-binding", "list", "--project", "7", "--json"],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let (m, t, _) = last(&srv);
    assert_eq!(
        (m.as_str(), t.as_str()),
        ("GET", "/api/v1/projects/7/integration-bindings")
    );
    let o = gstn(
        &sb,
        &srv,
        &[
            "integration-binding",
            "set",
            "--project",
            "7",
            "--integration",
            "3",
            "--external-project",
            "kanflow:42",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let (m, t, body) = last(&srv);
    assert_eq!(
        (m.as_str(), t.as_str()),
        ("PUT", "/api/v1/projects/7/integration-bindings/3")
    );
    let sent: Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(sent, json!({"external_project_id": "kanflow:42"}));
    assert_eq!(o.json()["outcome"], "bound");

    // Destructive: --yes first, before any request.
    let before = srv.seen().len();
    assert_eq!(
        gstn(
            &sb,
            &srv,
            &[
                "integration-binding",
                "remove",
                "--project",
                "7",
                "--integration",
                "3"
            ]
        )
        .code,
        2
    );
    assert_eq!(srv.seen().len(), before);
    let o = gstn(
        &sb,
        &srv,
        &[
            "integration-binding",
            "remove",
            "--project",
            "7",
            "--integration",
            "3",
            "--yes",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let (m, t, _) = last(&srv);
    assert_eq!(
        (m.as_str(), t.as_str()),
        ("DELETE", "/api/v1/projects/7/integration-bindings/3")
    );
    assert_eq!(
        o.json(),
        json!({"project_id": 7, "integration_id": 3, "unbound": true})
    );
}
