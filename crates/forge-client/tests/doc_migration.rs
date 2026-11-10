//! End-to-end tests of `gstn doc migration …` (plan 13 Q13) against a
//! scripted loopback server: the route and body each command sends, the
//! text for a date in force and an ended one, the confirmation of a clear,
//! an error answer shown as its code and request id, and nothing sent for
//! bad input. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

const IN_FORCE: &str = r#"{"project_id":7,"warn_until":"2027-03-31","in_force":true,"set_by":3,"set_at":"2026-11-10T01:02:03.000000Z","today":"2026-11-10","max_until":"2027-05-09","viewer":{"can_manage":true}}"#;
const ENDED: &str = r#"{"project_id":7,"warn_until":"2026-11-01","in_force":false,"set_by":3,"set_at":"2026-10-01T01:02:03.000000Z","today":"2026-11-10","max_until":"2027-05-09","viewer":{"can_manage":false}}"#;
const NONE: &str = r#"{"project_id":7,"warn_until":null,"in_force":false,"set_by":null,"set_at":null,"today":"2026-11-10","max_until":"2027-05-09","viewer":{"can_manage":false}}"#;

#[test]
fn doc_migration_set_puts_the_date() {
    let srv = Mock::start(|_| Resp::json(200, IN_FORCE));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "doc",
            "migration",
            "set",
            "--project",
            "7",
            "--until",
            "2027-03-31",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("migrating: until 2027-03-31"),
        "{}",
        o.stdout
    );
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("PUT", "/api/v1/projects/7/doc-lint/migration")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"warn_until": "2027-03-31"})
    );
}

#[test]
fn doc_migration_commands_call_their_routes() {
    let cases: Vec<(Vec<&str>, &str, u16, &str)> = vec![
        (
            vec!["doc", "migration", "show", "--project", "7"],
            "GET",
            200,
            IN_FORCE,
        ),
        (
            vec!["doc", "migration", "clear", "--project", "7", "--yes"],
            "DELETE",
            204,
            "",
        ),
    ];
    for (args, method, status, answer) in cases {
        let srv = Mock::start(move |_| Resp::json(status, answer));
        let sb = Sandbox::new();
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 0, "{args:?}: {}", o.stderr);
        let r = srv.seen().pop().expect("a request");
        assert_eq!(
            (r.method.as_str(), r.target.as_str()),
            (method, "/api/v1/projects/7/doc-lint/migration"),
            "{args:?}"
        );
    }
}

#[test]
fn doc_migration_show_says_whether_it_is_in_force() {
    for (answer, want) in [
        (IN_FORCE, "migrating: until 2027-03-31"),
        (ENDED, "ended on 2026-11-01"),
        (NONE, "Broken references are errors (no migration)."),
    ] {
        let srv = Mock::start(move |_| Resp::json(200, answer));
        let sb = Sandbox::new();
        let o = gstn(&sb, &srv, &["doc", "migration", "show", "--project", "7"]);
        assert_eq!(o.code, 0, "{}", o.stderr);
        assert!(o.stdout.contains(want), "{want}: {}", o.stdout);
        assert!(o.stdout.contains("Server date 2026-11-10"), "{}", o.stdout);
    }
    let srv = Mock::start(|_| Resp::json(200, IN_FORCE));
    let sb = Sandbox::new();
    let json = gstn(
        &sb,
        &srv,
        &["doc", "migration", "show", "--project", "7", "--json"],
    )
    .json();
    assert_eq!(json["warn_until"], "2027-03-31");
}

#[test]
fn a_refused_date_shows_the_code_and_request_id() {
    let srv = Mock::start(|_| {
        Resp::json(
            400,
            r#"{"error_code":"FG-DOC-0005","user_message":"The migration's end date must be between tomorrow and 180 days from today.","request_id":"req-synthetic-1"}"#,
        )
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "doc",
            "migration",
            "set",
            "--project",
            "7",
            "--until",
            "2030-01-01",
        ],
    );
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("FG-DOC-0005"), "{}", o.stderr);
    assert!(o.stderr.contains("req-synthetic-1"), "{}", o.stderr);
}

#[test]
fn doc_migration_refuses_bad_input_before_any_request() {
    let srv = Mock::start(|_| Resp::json(200, "{}"));
    let sb = Sandbox::new();
    for args in [
        vec!["doc", "migration", "clear", "--project", "7"],
        vec!["doc", "migration", "show", "--project", "0"],
        vec!["doc", "migration", "show"],
        vec!["doc", "migration", "set", "--project", "7"],
        vec!["doc", "migration", "set", "--project", "7", "--until", " "],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_ne!(o.code, 0, "{args:?}");
    }
    assert!(srv.seen().is_empty(), "nothing was sent");
}
