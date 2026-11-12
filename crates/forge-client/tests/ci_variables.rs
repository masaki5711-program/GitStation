//! End-to-end tests of `gstn ci variable …` (M4 C6) against a scripted
//! loopback server: the routes and bodies each command sends, the value
//! read from standard input (never an argument, never printed), the
//! confirmation of a delete, and nothing sent for a bad name. All data is
//! synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str], stdin: &str) -> Out {
    sb.run_env(&srv.url, args, stdin, &[("GSTN_TOKEN", TOKEN)])
}

const VAR: &str = r#"{"name":"DEPLOY_TOKEN","protected_only":true,"masked":true,"value_bytes":19,"created_by":3,"updated_by":3,"created_at":"2026-11-12T00:00:00Z","updated_at":"2026-11-12T00:00:00Z"}"#;

#[test]
fn ci_variable_set_sends_the_value_from_stdin_and_never_prints_it() {
    let srv = Mock::start(move |_| Resp::json(201, VAR));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &["ci", "variable", "set", "--project", "7", "DEPLOY_TOKEN"],
        "s3cr3t-deploy-value\n",
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(!o.stdout.contains("s3cr3t"), "{}", o.stdout);
    assert!(!o.stderr.contains("s3cr3t"), "{}", o.stderr);
    assert!(o.stdout.contains("DEPLOY_TOKEN masked"), "{}", o.stdout);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("PUT", "/api/v1/projects/7/ci/variables/DEPLOY_TOKEN")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"value": "s3cr3t-deploy-value", "masked": true})
    );
}

#[test]
fn ci_variable_list_and_delete_call_their_routes() {
    let list = format!(r#"{{"variables":[{VAR}]}}"#);
    let srv = Mock::start(move |r| {
        if r.method == "GET" {
            Resp::json(200, &list)
        } else {
            Resp::json(204, "")
        }
    });
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["ci", "variable", "list", "--project", "7"], "");
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("DEPLOY_TOKEN masked protected 19 bytes"),
        "{}",
        o.stdout
    );
    let o = gstn(
        &sb,
        &srv,
        &[
            "ci",
            "variable",
            "delete",
            "--project",
            "7",
            "DEPLOY_TOKEN",
            "--yes",
        ],
        "",
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let seen: Vec<(String, String)> = srv
        .seen()
        .into_iter()
        .map(|r| (r.method, r.target))
        .collect();
    assert_eq!(
        seen,
        [
            (
                "GET".to_owned(),
                "/api/v1/projects/7/ci/variables".to_owned()
            ),
            (
                "DELETE".to_owned(),
                "/api/v1/projects/7/ci/variables/DEPLOY_TOKEN".to_owned()
            ),
        ]
    );
}

#[test]
fn a_bad_name_an_empty_value_or_an_unconfirmed_delete_sends_nothing() {
    let srv = Mock::start(move |_| Resp::json(200, VAR));
    let sb = Sandbox::new();
    for (args, stdin) in [
        (
            vec!["ci", "variable", "set", "--project", "7", "../../admin"],
            "value-123",
        ),
        (
            vec!["ci", "variable", "set", "--project", "7", "DEPLOY_TOKEN"],
            "",
        ),
        (
            vec!["ci", "variable", "delete", "--project", "7", "DEPLOY_TOKEN"],
            "",
        ),
    ] {
        let o = gstn(&sb, &srv, &args, stdin);
        assert_ne!(o.code, 0, "{args:?}");
    }
    assert!(srv.seen().is_empty());
}
