//! End-to-end tests of `gstn mr …` (M3 R1a) against a scripted loopback
//! server. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

const MR: &str = r#"{"project_id":7,"iid":3,"title":"Add the login form","description":"synthetic","state":"open","draft":true,"source_branch":"feature/login","target_branch":"main","author_id":12,"head_sha":null,"created_at":"2026-11-02T00:00:00Z","updated_at":"2026-11-02T00:00:00Z","closed_at":null,"closed_by":null}"#;

fn ok(body: &str) -> Resp {
    Resp::json(200, body)
}

#[test]
fn mr_create_posts_the_branches_in_the_body_and_renders_the_result() {
    let srv = Mock::start(|_| Resp::json(201, MR));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "create",
            "--project",
            "7",
            "--source",
            "feature/login",
            "--target",
            "main",
            "--title",
            "Add the login form",
            "--draft",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("POST", "/api/v1/projects/7/merge-requests")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"title": "Add the login form", "source_branch": "feature/login",
               "target_branch": "main", "draft": true})
    );
    assert_eq!(o.json()["iid"], 3);
    let h = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "create",
            "--project",
            "7",
            "--source",
            "feature/login",
            "--target",
            "main",
            "--title",
            "x",
            "--description",
            "",
        ],
    );
    assert_eq!(h.code, 0, "{}", h.stderr);
    assert!(h.stdout.contains("!3") && h.stdout.contains("feature/login -> main"));
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json")["description"],
        ""
    );
}

#[test]
fn mr_list_filters_by_state_pages_with_before_and_all_follows_next_before() {
    let srv = Mock::start(|r| {
        if r.target.contains("before=3") {
            ok(
                r#"{"merge_requests":[{"project_id":7,"iid":1,"title":"old","state":"closed","draft":false,"source_branch":"a","target_branch":"main","author_id":12}],"next_before":null}"#,
            )
        } else {
            ok(&format!(r#"{{"merge_requests":[{MR}],"next_before":3}}"#))
        }
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "list",
            "--project",
            "7",
            "--state",
            "all",
            "--limit",
            "1",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        srv.seen().pop().expect("a request").target,
        "/api/v1/projects/7/merge-requests?state=all&limit=1"
    );
    assert_eq!(o.json()["next_before"], 3);
    let h = gstn(&sb, &srv, &["mr", "list", "--project", "7"]);
    assert_eq!(h.code, 0, "{}", h.stderr);
    assert!(h.stdout.contains("more: --before 3"), "{}", h.stdout);
    let all = gstn(
        &sb,
        &srv,
        &["mr", "list", "--project", "7", "--all", "--json"],
    );
    assert_eq!(all.code, 0, "{}", all.stderr);
    let iids: Vec<i64> = all.json()["merge_requests"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|m| m["iid"].as_i64().expect("iid"))
        .collect();
    assert_eq!(iids, [3, 1]);
    // An unknown state never reaches the server.
    let before = srv.seen().len();
    let bad = gstn(
        &sb,
        &srv,
        &["mr", "list", "--project", "7", "--state", "draft"],
    );
    assert_eq!(bad.code, 2);
    assert_eq!(srv.seen().len(), before);
}

#[test]
fn mr_view_edit_close_and_reopen_call_their_routes() {
    let srv = Mock::start(|_| ok(MR));
    let sb = Sandbox::new();
    for (args, method, target) in [
        (
            vec!["mr", "view", "--project", "7", "--iid", "3"],
            "GET",
            "/api/v1/projects/7/merge-requests/3",
        ),
        (
            vec!["mr", "close", "--project", "7", "--iid", "3"],
            "POST",
            "/api/v1/projects/7/merge-requests/3/close",
        ),
        (
            vec!["mr", "reopen", "--project", "7", "--iid", "3"],
            "POST",
            "/api/v1/projects/7/merge-requests/3/reopen",
        ),
    ] {
        let o = gstn(&sb, &srv, &args);
        assert_eq!(o.code, 0, "{args:?}: {}", o.stderr);
        let r = srv.seen().pop().expect("a request");
        assert_eq!((r.method.as_str(), r.target.as_str()), (method, target));
    }
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "edit",
            "--project",
            "7",
            "--iid",
            "3",
            "--ready",
            "--target",
            "release",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let r = srv.seen().pop().expect("a request");
    assert_eq!(
        (r.method.as_str(), r.target.as_str()),
        ("PATCH", "/api/v1/projects/7/merge-requests/3")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).expect("json"),
        json!({"target_branch": "release", "draft": false})
    );
    // Nothing to change, or both flags: refused before any request.
    let before = srv.seen().len();
    let none = gstn(
        &sb,
        &srv,
        &["mr", "edit", "--project", "7", "--iid", "3", "--json"],
    );
    assert_eq!(none.code, 2);
    assert!(none.err_json()["detail"]
        .as_str()
        .expect("detail")
        .contains("nothing_to_change"));
    let both = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "edit",
            "--project",
            "7",
            "--iid",
            "3",
            "--draft",
            "--ready",
        ],
    );
    assert_eq!(both.code, 2);
    let bad_iid = gstn(&sb, &srv, &["mr", "view", "--project", "7", "--iid", "0"]);
    assert_eq!(bad_iid.code, 2);
    assert_eq!(srv.seen().len(), before);
}

#[test]
fn mr_errors_keep_the_server_code_and_exit_class() {
    let srv = Mock::start(|_| {
        Resp::json(
            409,
            r#"{"error_code":"FG-REVIEW-0001","user_message":"同じブランチどうしの開いているマージリクエストがすでにあります。","request_id":"req-9"}"#,
        )
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "mr",
            "create",
            "--project",
            "7",
            "--source",
            "a",
            "--target",
            "main",
            "--title",
            "x",
            "--json",
        ],
    );
    assert_eq!(o.code, 5, "a conflict: {}", o.stderr);
    assert_eq!(o.err_json()["error_code"], "FG-REVIEW-0001");
    assert_eq!(o.err_json()["request_id"], "req-9");
    assert!(!o.stderr.contains(TOKEN));
}
