//! End-to-end tests of `gstn mirror …` (M3 O2) against a scripted loopback
//! server: the route and body each command sends, the confirmation of a
//! removal, an error answer shown as its code and request id, and nothing
//! sent for bad input. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

const MIRROR: &str = r#"{"repo_id":9,"url":"https://gitlab.example.invalid/ymma/app.git","origin":"https://gitlab.example.invalid:443","credential_name":"gitlab","enabled":true,"created_by":1,"created_at":"2026-11-07T00:00:00Z","updated_at":"2026-11-07T00:00:00Z","status":{"pending_seq":4,"pending_since":"2026-11-07T00:01:00Z","mirrored_seq":3,"last_attempt_at":"2026-11-07T00:01:05Z","last_success_at":"2026-11-07T00:00:30Z","failures":1,"last_error_code":"FG-INT-0013","last_reason":"auth"}}"#;

#[test]
fn mirror_set_puts_the_url_credential_and_enabled_flag() {
    for (extra, enabled) in [(None, true), (Some("--disabled"), false)] {
        let srv = Mock::start(|_| Resp::json(201, MIRROR));
        let sb = Sandbox::new();
        let mut args = vec![
            "mirror",
            "set",
            "--project",
            "7",
            "--url",
            "https://gitlab.example.invalid/ymma/app.git",
            "--credential",
            "gitlab",
        ];
        args.extend(extra);
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 0, "{}", o.stderr);
        assert!(o.stdout.contains("Mirror set"), "{}", o.stdout);
        let r = srv.seen().pop().expect("a request");
        assert_eq!(
            (r.method.as_str(), r.target.as_str()),
            ("PUT", "/api/v1/projects/7/push-mirror")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&r.body).expect("json"),
            json!({
                "url": "https://gitlab.example.invalid/ymma/app.git",
                "credential_name": "gitlab",
                "enabled": enabled,
            })
        );
    }
}

#[test]
fn mirror_commands_call_their_routes() {
    let cases: Vec<(Vec<&str>, &str, &str, u16, &str)> = vec![
        (
            vec!["mirror", "show", "--project", "7"],
            "GET",
            "/api/v1/projects/7/push-mirror",
            200,
            MIRROR,
        ),
        (
            vec!["mirror", "sync", "--project", "7"],
            "POST",
            "/api/v1/projects/7/push-mirror/sync",
            202,
            r#"{"queued":true}"#,
        ),
        (
            vec!["mirror", "remove", "--project", "7", "--yes"],
            "DELETE",
            "/api/v1/projects/7/push-mirror",
            204,
            "",
        ),
    ];
    for (args, method, target, status, answer) in cases {
        let srv = Mock::start(move |_| Resp::json(status, answer));
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
fn mirror_show_prints_the_progress_and_the_last_failure() {
    let srv = Mock::start(|_| Resp::json(200, MIRROR));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["mirror", "show", "--project", "7"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    for want in [
        "https://gitlab.example.invalid/ymma/app.git",
        "credential gitlab (enabled)",
        "waiting since 2026-11-07T00:01:00Z",
        "failures 1",
        "last failure FG-INT-0013 reason auth",
    ] {
        assert!(o.stdout.contains(want), "{want}: {}", o.stdout);
    }
    let json = gstn(&sb, &srv, &["mirror", "show", "--project", "7", "--json"]).json();
    assert_eq!(json["status"]["failures"], 1);
}

#[test]
fn mirror_sync_says_when_the_push_was_already_queued() {
    let srv = Mock::start(|_| Resp::json(202, r#"{"queued":false}"#));
    let sb = Sandbox::new();
    let o = gstn(&sb, &srv, &["mirror", "sync", "--project", "7"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("already queued"), "{}", o.stdout);
}

#[test]
fn a_refused_mirror_shows_the_code_and_request_id() {
    let srv = Mock::start(|_| {
        Resp::json(
            400,
            r#"{"error_code":"FG-INT-0021","user_message":"The mirror destination or credential name is not valid.","request_id":"req-synthetic-1"}"#,
        )
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mirror",
            "set",
            "--project",
            "7",
            "--url",
            "https://elsewhere.example.invalid/x.git",
            "--credential",
            "gitlab",
        ],
    );
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("FG-INT-0021"), "{}", o.stderr);
    assert!(o.stderr.contains("req-synthetic-1"), "{}", o.stderr);
}

#[test]
fn mirror_refuses_bad_input_before_any_request() {
    let srv = Mock::start(|_| Resp::json(200, "{}"));
    let sb = Sandbox::new();
    for args in [
        vec!["mirror", "remove", "--project", "7"],
        vec!["mirror", "show", "--project", "0"],
        vec!["mirror", "show"],
        vec![
            "mirror",
            "set",
            "--project",
            "7",
            "--url",
            "https://x/y.git",
        ],
        vec!["mirror", "set", "--project", "7", "--credential", "gitlab"],
        vec![
            "mirror",
            "set",
            "--project",
            "7",
            "--url",
            " ",
            "--credential",
            "gitlab",
        ],
        vec!["mirror", "sync", "--project", "x"],
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_ne!(o.code, 0, "{args:?}");
    }
    assert!(srv.seen().is_empty(), "nothing was sent");
}
